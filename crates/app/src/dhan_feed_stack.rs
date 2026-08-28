//! Dhan 16-connection live-feed stack — boot wiring, DEFAULT-OFF.
//!
//! **Authority:** the operator's dated 2026-08-09 second quote in
//! `.claude/rules/project/websocket-connection-scope-lock.md`, section
//! "2026-08-09 (SAME DAY, SECOND QUOTE) — 16 CONNECTIONS + depth-20/depth-200
//! AUTHORIZED", which raises the main-feed pool from 1 to 5, un-forbids
//! depth-20 and depth-200 at 5 each, and leaves order-update at 1 — sixteen
//! sockets in total. Companion plan:
//! `.claude/plans/proposals/2026-08-09-dhan-16-connection-architecture.md`.
//!
//! This module is the boot seam: it decides whether the lane may run at all,
//! shards the authorized instrument set across the pools, reserves the
//! connection budget, and hands the assembled shape to
//! [`tickvault_core::websocket::pool_supervisor`]. It mirrors the shape of
//! [`crate::dhan_rest_stack`] — a `spawn_*` returning an optional
//! `JoinHandle`, a bring-up body that never blocks boot and never halts the
//! process.
//!
//! # Default OFF, twice over
//! Nothing here runs unless BOTH gates open:
//!
//! 1. `[feeds] dhan_enabled = true` — the documented re-enable path
//!    (`websocket-connection-scope-lock.md`: "a Dhan re-enable is config +
//!    restart + a fresh dated quote"). It is `false` in `config/base.toml`
//!    **and** in `config/production.toml` today.
//! 2. The environment opt-in [`DHAN_LIVE_FEED_ENV`] set to `1`.
//!
//! The second gate exists because `FeedsConfig`'s *struct* default for
//! `dhan_enabled` is `true` — a boot that somehow deserialized without a
//! `[feeds]` section would otherwise open sixteen sockets to a broker whose
//! live feed the operator retired on 2026-07-13. An absent environment
//! variable is default-OFF **by construction**, with no file to get wrong.
//! The pattern follows the house precedent for operator-triggered,
//! never-scheduled behaviour (`TICKVAULT_GROWW_RATE_PROBE`).
//!
//! With either gate shut this module allocates nothing, spawns nothing, and
//! opens nothing: behaviour is byte-identical to a build without it.
//!
//! # Hardcoded instruments, never a CSV
//! Q3 of the 2026-07-13 retirement amendment stands and is NOT lifted by the
//! 2026-08-09 quote: "hereafter no Dhan instrument download/parsing — just
//! direct hardcoded security IDs". [`hardcoded_index_universe`] therefore
//! reads the pinned [`SPOT_1M_REST_INDICES`] table; there is no downloader,
//! no parser, and no path by which one could be reached from here.
//!
//! # The path one tick takes
//! ```text
//! socket ──▶ WalRingSink ──▶ WAL (durable)  ──▶ bounded ring ──▶ run_frame_drain
//!            (read task)      then, only then      65,536         (its own task)
//!                                                                       │
//!                              gap detector ◀── LiveIngest::ingest_tick ─┘
//!                              aggregator (24 timeframes) ──▶ seal ring
//!                              TickWriter::append_tick_with_seq ──▶ ticks
//! ```
//! The split at the ring is the whole design. The read task does exactly one
//! thing per frame — hand it to the sink — because anything else it did would
//! stall the automatic pong and turn a slow fold into a disconnect. The frame
//! is durable in the write-ahead log BEFORE it is visible to the fold, so a
//! process kill between the two steps loses nothing; a full ring is therefore
//! back-pressure, never capture loss.
//!
//! # Honest state of this round
//! Sockets are dialed, frames are captured, and the fold consumes them.
//! `tv_dhan_feed_stack_up` reads `1` only once sockets are dialed AND the
//! drain is running, and the drain itself clears it when the last sender dies
//! — the gauge tracks the lane carrying data, never "config was enabled"
//! (audit Rule 11, no false-OK). The lane refuses to open a single socket
//! without a write-ahead log or without a registered token manager, because
//! capturing without a durable floor, or dialing with a blank credential,
//! would both look like success while being neither.
//!
//! **NOT claimed:** that a tick was ever observed arriving. Every branch here
//! is exercised against a fake transport and pure unit tests; no session has
//! run against live Dhan since the 2026-07-13 retirement. The main feed
//! carries no sequence number and no snapshot-on-subscribe, so packet loss is
//! undetectable at the protocol level — the 15:31 REST cross-verification is
//! the lane's only ground truth, and it is spawned inside the same gate for
//! exactly that reason. The delivery-lag and silent-instrument problems that
//! caused the retirement (p99 46 s, max 199 s, 29–67 silent instruments per
//! minute — `websocket-connection-scope-lock.md` §E) are Dhan-side and are
//! NOT fixed by any of this.

use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use secrecy::ExposeSecret;
use tickvault_common::config::QuestDbConfig;
use tickvault_common::constants::{
    DHAN_MAIN_FEED_WS_BASE_URL, DHAN_TWENTY_DEPTH_WS_BASE_URL, DHAN_TWO_HUNDRED_DEPTH_WS_BASE_URL,
    MAX_PLAUSIBLE_LTP, SPOT_1M_REST_INDICES, TICK_PERSIST_END_SECS_OF_DAY_IST,
};
use tickvault_common::error_code::ErrorCode;
use tickvault_common::feed::Feed;
use tickvault_common::ingest_shed::INGEST_SHED;
use tickvault_common::tick_types::ParsedTick;
use tickvault_common::types::{ExchangeSegment, SecurityId};
use tickvault_core::auth::token_manager::global_token_manager;
use tickvault_core::parser::ParsedFrame;
use tickvault_core::parser::depth::{
    DepthFeedKind, DepthLevelBuffer, DepthPayload, DepthSide, DepthSplitStop, parse_depth_packet,
    split_depth_frame,
};
use tickvault_core::parser::dispatcher::dispatch_frame;
use tickvault_core::pipeline::tick_gap_detector::{
    DetectorConfig, SilenceVerdict, TickGapDetector, TickObservation,
};
use tickvault_core::websocket::connection::{
    DhanFeedSocketImpl, DhanSocketParams, FeedTokenBuffer,
};
use tickvault_core::websocket::pool_budget::{
    ConnectionSlot, DhanEndpointType, MAX_TOTAL_DHAN_CONNECTIONS,
};
use tickvault_core::websocket::pool_supervisor::{
    CapturedFrame, ConnectionSupervisor, LiveSubscriptionCommand, PoolSupervisor, RingByteBudget,
    SubscribeGuard, SubscribeGuardRefusal, SubscribeInstrument, WalRingSink,
    run_connection_with_commands,
};
use tickvault_storage::depth_persistence::{
    DEPTH_KIND_5, DEPTH_KIND_20, DEPTH_KIND_200, DEPTH_SIDE_ASK, DEPTH_SIDE_BID, DepthRow,
    DepthWriter, depth_segment_label,
};
use tickvault_storage::tick_persistence::TickWriter;
use tickvault_storage::ws_frame_spill::{WsFrameSpill, WsType};
use tickvault_trading::candles::multi_tf_aggregator::AGGREGATOR_MAX_SLOTS;
use tickvault_trading::candles::{BufferedSeal, ConsumeStats, FeedStrategy, MultiTfAggregator};
use tracing::{error, info, warn};

/// Environment opt-in that must be `1` for the lane to run, on top of
/// `[feeds] dhan_enabled`. Absent means OFF, which is the whole point.
pub const DHAN_LIVE_FEED_ENV: &str = "TICKVAULT_DHAN_LIVE_FEED";

/// The only accepted value of [`DHAN_LIVE_FEED_ENV`]. Anything else — `true`,
/// `yes`, `on`, empty — is treated as OFF rather than guessed at.
pub const DHAN_LIVE_FEED_ENV_ON: &str = "1";

/// Gauge: `1` only when the lane is actually carrying data. Pinned at `0`
/// through bring-up and for as long as the transport is unwired.
pub const FEED_STACK_UP_GAUGE: &str = "tv_dhan_feed_stack_up";

/// Gauge: connections the plan reserved, by endpoint type.
///
/// PLANNED, not alive. It is written once at bring-up and never moves again, so
/// it answers "what did we intend?" and cannot answer "what is still running?".
/// Use [`ALIVE_CONNECTIONS_GAUGE`] for the latter.
pub const FEED_STACK_CONNECTIONS_GAUGE: &str = "tv_dhan_feed_stack_connections";

/// Gauge: sockets whose supervisor task is still running, right now.
///
/// 2026-08-14. The lane had exactly two liveness signals and neither could see
/// a PARTIAL failure: `tv_dhan_feed_stack_up` is cleared only when the ring
/// closes — which needs EVERY sender dropped — and the planned-connections
/// gauge is a boot-time constant. So four of five main-feed sockets could park
/// and both signals would still read healthy while ~80% of the universe went
/// dark. The park counter fires on the transition, but a counter cannot answer
/// "how many are up right now", and a delta that already scrolled past is not
/// a state anyone can query at 09:30.
pub const ALIVE_CONNECTIONS_GAUGE: &str = "tv_dhan_ws_alive_connections";

/// Live count behind [`ALIVE_CONNECTIONS_GAUGE`].
static ALIVE_CONNECTIONS: AtomicUsize = AtomicUsize::new(0);

/// Longest ring dwell seen since the last publish, in NANOSECONDS.
///
/// A module static rather than a `run_frame_drain` local so that
/// [`publish_fold_depth`] can drain it without a signature change at its four
/// call sites — the same shape as [`ALIVE_CONNECTIONS`] above, for the same
/// reason.
///
/// Exactly ONE writer exists (the drain task) and one reader (the publish, on
/// that same task), so `Relaxed` is sound: there is no other thread whose
/// writes this value must be ordered against. An uncontended `fetch_max` at
/// the ~5,000 frames/sec envelope is a few nanoseconds and no allocation —
/// the cost this module's `DrainCounters` docs exist to keep honest.
///
/// Nanoseconds internally, milliseconds at the gauge: the gauge is read by a
/// human deciding whether the drain is behind, and no human needs nanosecond
/// resolution to answer that. Storing nanos avoids a division per frame.
static RING_DWELL_MAX_NANOS: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);

/// Records one frame's ring dwell. Called once per frame; keeps the maximum.
///
/// Free function rather than an inline `fetch_max` at the call site so the
/// unit conversion and the ordering choice live in ONE place with their
/// reasoning, instead of as a bare atomic call a later reader has to re-derive.
pub fn record_ring_dwell(nanos: i64) {
    RING_DWELL_MAX_NANOS.fetch_max(nanos, Ordering::Relaxed);
}

/// Takes the maximum dwell and RESETS it, returning milliseconds.
///
/// Reset-on-read deliberately: a sticky maximum reads alarming forever after a
/// single stall, and a signal that is permanently red is a signal nobody
/// reads. Each published value therefore means "the worst dwell in the window
/// that just ended", which is the question an operator is actually asking.
pub fn take_ring_dwell_max_ms() -> f64 {
    let nanos = RING_DWELL_MAX_NANOS.swap(0, Ordering::Relaxed);
    // `u32::try_from` then `f64::from`: lossless by construction, and no lossy
    // `as` cast to justify — the same wording, and the same reason, as
    // `publish_fold_depth` a few hundred lines down.
    //
    // The first draft here was `nanos as f64` under an
    // `#[allow(clippy::cast_precision_loss)]`, and the banned-pattern scanner
    // refused it for want of an `// APPROVED:` line. Silencing the lint would
    // have been the wrong answer twice over: the cast really is lossy above
    // 2^53, and the sibling function three screens away already shows the
    // lossless form. An `#[allow]` is a claim that no better shape exists, and
    // one did.
    //
    // Nanos -> whole MICROSECONDS first, so the u32 window is wide enough to
    // matter: `u32::MAX` micros is ~71 minutes of dwell. Saturating there is
    // not a limitation worth avoiding — a drain 71 minutes behind has been
    // catastrophically broken for over an hour, and every other signal on the
    // dashboard is screaming by then. Truncating sub-microsecond dwell is
    // likewise deliberate: nobody deciding "is the drain behind?" needs
    // nanosecond resolution, and the gauge is read by a human.
    let micros = u32::try_from(nanos / 1_000).unwrap_or(u32::MAX);
    f64::from(micros) / 1_000.0
}

/// RAII counter for [`ALIVE_CONNECTIONS`].
///
/// The increment happens OUTSIDE the socket task, deliberately, so the gauge
/// can never read low because a spawn lost a race with its own decrement. That
/// asymmetry is what makes a guard necessary rather than tidy: a decrement
/// written as a plain statement at the end of the task body is skipped on
/// unwind, and the gauge then reports N sockets alive when N−1 are, forever,
/// with nothing to correct it. Release builds are `panic = "abort"` so today
/// that only bites in debug and test — the guard makes it structural instead
/// of relying on that profile setting staying put.
///
/// # Honest limit
///
/// The atomic update and the gauge publish are two steps, so two threads
/// transitioning at once can latch the LATER-published of two valid readings —
/// a transiently stale count, self-correcting on the next transition. Closing
/// that would need a lock around a monitoring write, which is a worse trade
/// than a reading that is briefly one socket out of date.
struct AliveConnectionGuard {
    armed: bool,
}

impl AliveConnectionGuard {
    /// Counts one socket alive and publishes the new total.
    fn acquire() -> Self {
        publish_alive_connections(ALIVE_CONNECTIONS.fetch_add(1, Ordering::SeqCst) + 1);
        Self { armed: true }
    }

    /// Normal path: consume the guard, returning the remaining count so the
    /// caller can log it. Disarms first, so `Drop` cannot double-decrement.
    fn release(mut self) -> usize {
        self.armed = false;
        Self::decrement()
    }

    fn decrement() -> usize {
        let remaining = ALIVE_CONNECTIONS
            .fetch_sub(1, Ordering::SeqCst)
            .saturating_sub(1);
        publish_alive_connections(remaining);
        remaining
    }
}

impl Drop for AliveConnectionGuard {
    fn drop(&mut self) {
        if self.armed {
            Self::decrement();
        }
    }
}

/// Where the lane reports its live socket count, so `/health` can answer
/// "is the feed alive?" instead of "that subsystem was retired".
///
/// `SystemHealthStatus` gates its websocket row on whether ANY producer has
/// ever pushed a count. That gate was added on 2026-08-09 for a good reason —
/// with the lane deleted, the count sat at 0 forever and `/health` returned
/// `degraded` on every single request, which is a verdict carrying no
/// information. Its doc says the flag is "arm-on-arrival": the moment the
/// revived lane pushes a count, the row reports for real with no edit needed
/// on the API side.
///
/// The lane was revived (operator quotes 2026-08-09, default ON 2026-08-11)
/// and never pushed. So on a box with `dhan_enabled = true` and sixteen
/// sockets dialing, `/health` still rendered:
///
/// ```json
/// "websocket": { "status": "retired", "detail": "live feeds retired 2026-07-13/15" }
/// ```
///
/// A dead lane and a healthy lane rendered identically, on the endpoint
/// scripts poll to tell them apart. This installs the missing producer.
static HEALTH_REPORTER: std::sync::OnceLock<tickvault_api::state::SharedHealthStatus> =
    std::sync::OnceLock::new();

/// Install the `/health` websocket reporter. Returns `false` if one was
/// already installed — first writer wins, same shape as
/// [`install_crossverify_deps`].
pub fn install_health_reporter(health: tickvault_api::state::SharedHealthStatus) -> bool {
    HEALTH_REPORTER.set(health).is_ok()
}

/// The process-wide feed-health registry, reachable from the socket-count
/// choke point.
///
/// # Why a `OnceLock` and not another `params` field
///
/// `params.feed_health` already exists and is threaded to the drain, but
/// `publish_alive_connections` is a free function on BOTH edges of
/// [`AliveConnectionGuard`] — that is the whole reason its own doc calls it
/// "one function owns the health push, so there is no second path to drift".
/// Threading an `Arc` down to a `Drop` impl would either add a field to the
/// guard or add a second push site. This mirrors `HEALTH_REPORTER` directly
/// above, which solved the identical problem for the same function.
static FEED_HEALTH: std::sync::OnceLock<Arc<tickvault_common::feed_health::FeedHealthRegistry>> =
    std::sync::OnceLock::new();

/// Install the feed-health registry. Returns `false` if one was already
/// installed — first writer wins, same shape as [`install_health_reporter`].
pub fn install_feed_health(health: Arc<tickvault_common::feed_health::FeedHealthRegistry>) -> bool {
    FEED_HEALTH.set(health).is_ok()
}

/// Report whether the tick writer reached the database, for `/health`.
///
/// Sibling of the websocket row and the same defect: `/health` rendered
///
/// ```json
/// "tick_persistence": { "status": "retired", "detail": "tick writer deleted 2026-07-17" }
/// ```
///
/// while `crates/storage/src/tick_persistence.rs` exists (it came back with
/// the 2026-08-09 revival) and the lane appends every tick through it. The
/// setter had zero production callers, so the row's gate never armed.
///
/// # What this deliberately does NOT do
///
/// It does not arm the row at spawn. Before the first flush there is nothing
/// TRUE to report: constructing a `TickWriter` proves a client was built, not
/// that the database answered, and ILP is lazy enough that the two are
/// genuinely different facts. Reporting `connected` on that basis would be the
/// same over-claim this row is being fixed for, pointed the other way.
///
/// The residual is therefore real and stated: a lane that is up but has not
/// yet flushed a single row still reads `retired`. `LiveIngest::flush` returns
/// early when nothing is pending, so on a session with no ticks at all that
/// window never closes. What changed is that the retired branch no longer
/// asserts a deletion that was reversed, and that the row tells the truth from
/// the first row written onward.
fn report_tick_persistence(connected: bool) {
    if let Some(health) = HEALTH_REPORTER.get() {
        health.set_tick_persistence_connected(connected);
    }
}
/// Publish the alive-socket count -- to the gauge AND to `/health`.
///
/// Both edges of [`AliveConnectionGuard`] land here, so wiring the health push
/// into this one function tracks every transition with no extra timer and no
/// second source of truth to drift.
fn publish_alive_connections(alive: usize) {
    // `u32::try_from` then `f64::from`: lossless by construction (bounded by
    // the 16-socket lock) and no lossy `as` cast to justify.
    metrics::gauge!(ALIVE_CONNECTIONS_GAUGE)
        .set(f64::from(u32::try_from(alive).unwrap_or(u32::MAX)));
    if let Some(health) = HEALTH_REPORTER.get() {
        health.set_websocket_connections(u64::try_from(alive).unwrap_or(u64::MAX));
    }
    // THE THIRD SIBLING (2026-08-26).
    //
    // `FeedHealthRegistry::set_connected` had ZERO production call sites —
    // every reference in the workspace was inside a test-only module. The
    // field initialises `false`, and `feed_health::classify` tests
    // `if !i.connected` BEFORE it looks at tick age, so `/api/feeds/health`
    // returned `Down, "enabled but disconnected — reconnecting"`
    // unconditionally.
    //
    // Captured on prod at one instant, 2026-08-26:
    //
    // ```text
    // /health            overall=healthy, websocket={connected, "15 connections"}
    // /api/feeds/health  dhan verdict=DOWN connected=false
    //                    ticks_total=17,265,688  last_tick_age=1s
    // :9091              tv_dhan_ws_alive_connections 15
    //                    tv_dhan_feed_stack_up 1
    // ```
    //
    // The same JSON object declared the feed down while reporting 17.2 million
    // ticks and a one-second-old tick. `/board`, `/dashboard` and `/feeds` all
    // render that row, so the operator's at-a-glance surface cried wolf every
    // trading day, permanently — the inverse of a false-OK and no less
    // corrosive, because a status that is always red is read as decoration.
    //
    // This is the THIRD half of one defect, and the pattern is the point:
    // `set_dhan_lane_running` was fixed 2026-08-14 ("a status line that cannot
    // vary is not a status line") and `record_ticks` 2026-08-18 ("it answered a
    // benign Unknown for a corpse"). Each fix documented the shape and left the
    // next sibling in place. `sp5_dhan_feed_health_wiring_guard.rs:42` even
    // named this one and said it "MUST be re-pinned in the PR that wires them".
    //
    // Sited here rather than beside `set_dhan_lane_running` deliberately: this
    // function is both edges of `AliveConnectionGuard`, so it tracks real
    // socket transitions instead of a coarse lane-level flag, and it keeps the
    // single-owner property the function was built for.
    if let Some(feed_health) = FEED_HEALTH.get() {
        feed_health.set_connected(tickvault_common::feed::Feed::Dhan, alive > 0);
    }
}

/// Process-global once-guard: two feed stacks would fight over the same
/// sixteen-socket budget and Dhan would answer by killing the oldest sockets.
static FEED_STACK_SPAWNED: AtomicBool = AtomicBool::new(false);

// ---------------------------------------------------------------------------
// Gate (pure)
// ---------------------------------------------------------------------------

/// Why the lane is or is not permitted to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedStackGate {
    /// Both gates open.
    Enabled,
    /// `[feeds] dhan_enabled` is false — the documented default.
    DisabledByConfig,
    /// Config permits it but the environment opt-in is absent or not `1`.
    DisabledByEnv,
}

impl FeedStackGate {
    /// Whether the lane may run.
    #[must_use]
    pub const fn is_enabled(self) -> bool {
        matches!(self, Self::Enabled)
    }

    /// Stable lowercase tag for logs and metric labels.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Enabled => "enabled",
            Self::DisabledByConfig => "disabled_by_config",
            Self::DisabledByEnv => "disabled_by_env",
        }
    }
}

/// The gate decision. Pure: the environment is read by the caller and passed
/// in, so every combination is unit-testable without touching process state.
///
/// Config is checked FIRST so the message an operator sees names the gate they
/// are most likely to have meant.
#[must_use]
pub fn feed_stack_gate(config_dhan_enabled: bool, env_opt_in: Option<&str>) -> FeedStackGate {
    if !config_dhan_enabled {
        return FeedStackGate::DisabledByConfig;
    }
    match env_opt_in {
        Some(v) if v == DHAN_LIVE_FEED_ENV_ON => FeedStackGate::Enabled,
        _ => FeedStackGate::DisabledByEnv,
    }
}

// ---------------------------------------------------------------------------
// Universe (hardcoded — Q3 stands)
// ---------------------------------------------------------------------------

/// The main-feed instrument set: the four pinned index security ids
/// (`NIFTY 13`, `BANKNIFTY 25`, `SENSEX 51`, `INDIA VIX`), all `IDX_I`.
///
/// Read from [`SPOT_1M_REST_INDICES`] rather than re-typed, so the live-feed
/// lane and the REST legs can never drift onto different security ids for the
/// same index. Widening this set is a scope change requiring its own dated
/// operator quote — the 2026-08-09 quote authorized CONNECTIONS, not a larger
/// universe, and it explicitly left the no-CSV rule standing.
#[must_use]
pub fn hardcoded_index_universe() -> Vec<SubscribeInstrument> {
    SPOT_1M_REST_INDICES
        .iter()
        .map(|(security_id, _name)| SubscribeInstrument {
            security_id: *security_id,
            segment: ExchangeSegment::IdxI,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Plan
// ---------------------------------------------------------------------------

/// One connection the plan intends to open.
#[derive(Debug, Clone)]
pub struct PlannedConnection {
    /// Budget-granted slot (endpoint type, pool index, global index).
    pub slot: ConnectionSlot,
    /// The subscription this connection replays on every connect.
    pub guard: SubscribeGuard,
}

/// The whole sixteen-socket shape for one boot.
#[derive(Debug, Default)]
pub struct FeedStackPlan {
    /// Connections in admission order.
    pub connections: Vec<PlannedConnection>,
}

impl FeedStackPlan {
    /// Total connections planned.
    #[must_use]
    pub fn len(&self) -> usize {
        self.connections.len()
    }

    /// Whether the plan opens nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.connections.is_empty()
    }

    /// Connections planned for one endpoint type.
    ///
    /// O(N) with N ≤ 16 — a scan, labelled as one. Cold path, called for
    /// logging and gauges only.
    #[must_use]
    pub fn count_for(&self, endpoint: DhanEndpointType) -> usize {
        self.connections
            .iter()
            .filter(|c| c.slot.endpoint == endpoint)
            .count()
    }
}

/// Why a plan could not be built. Every variant is a refusal BEFORE any dial,
/// which is the fail-closed direction: Dhan does not reject an over-limit
/// socket, it silently kills the oldest one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FeedStackPlanError {
    /// The instrument set needs more connections than the endpoint type allows.
    PoolTooSmall {
        /// Endpoint type.
        endpoint: DhanEndpointType,
        /// Instruments requested.
        instruments: usize,
        /// Connections the shard would need.
        needed: usize,
        /// Connections the operator authorized.
        available: u8,
    },
    /// The connection budget refused a slot mid-plan.
    BudgetRefused {
        /// Endpoint type.
        endpoint: DhanEndpointType,
    },
    /// A shard exceeded the endpoint's per-connection instrument cap.
    SubscriptionRefused(SubscribeGuardRefusal),
}

impl core::fmt::Display for FeedStackPlanError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::PoolTooSmall {
                endpoint,
                instruments,
                needed,
                available,
            } => write!(
                f,
                "{endpoint} needs {needed} connections for {instruments} instruments \
                 but only {available} are authorized"
            ),
            Self::BudgetRefused { endpoint } => {
                write!(f, "{endpoint} budget refused a connection while planning")
            }
            Self::SubscriptionRefused(inner) => {
                write!(f, "subscription set refused: {inner}")
            }
        }
    }
}

impl std::error::Error for FeedStackPlanError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::SubscriptionRefused(inner) => Some(inner),
            Self::PoolTooSmall { .. } | Self::BudgetRefused { .. } => None,
        }
    }
}

impl From<SubscribeGuardRefusal> for FeedStackPlanError {
    fn from(value: SubscribeGuardRefusal) -> Self {
        Self::SubscriptionRefused(value)
    }
}

/// Shards the three market-data instrument sets across their pools and reserves
/// the connection budget for the result.
///
/// Sharding is a plain `chunks` over each set at the endpoint's documented
/// per-connection cap: 5,000 for the main feed, 50 for depth-20, and 1 for
/// depth-200 (a 200-level book is a whole connection's bandwidth on its own).
/// An empty set plans zero connections for that pool, which is the honest shape
/// today for depth — no operator-named depth instruments exist yet.
///
/// **O(n) in total instruments, not O(1)** — every instrument must be placed.
/// Cold path, once per boot, bounded by 5×5,000 + 5×50 + 5×1.
///
/// # Errors
/// [`FeedStackPlanError::PoolTooSmall`] when a set needs more connections than
/// the operator authorized; [`FeedStackPlanError::BudgetRefused`] if the shared
/// budget refuses a slot; [`FeedStackPlanError::SubscriptionRefused`] if a
/// shard somehow exceeds the per-connection cap.
pub fn build_feed_stack_plan(
    pool: &mut PoolSupervisor,
    now: std::time::Instant,
    main_feed: &[SubscribeInstrument],
    depth_20: &[SubscribeInstrument],
    depth_200: &[SubscribeInstrument],
) -> Result<FeedStackPlan, FeedStackPlanError> {
    let mut plan = FeedStackPlan::default();
    for (endpoint, set) in [
        (DhanEndpointType::MainFeed, main_feed),
        (DhanEndpointType::Depth20, depth_20),
        (DhanEndpointType::Depth200, depth_200),
    ] {
        let (deduped, duplicates) = dedup_subscribe_set(set);
        if duplicates > 0 {
            // Loud, because a duplicate is never harmless here. It burns one of
            // Dhan's per-connection wire slots, and it makes the planner's count
            // disagree with the aggregator's — which keys on
            // `(Feed, security_id, segment)` and collapses them to one slot. The
            // planner counting higher than the aggregator is how a set that
            // "fits" turns into refused slots at fold time.
            error!(
                code = ErrorCode::InstrumentP1CrossSegmentCollision.code_str(),
                endpoint = endpoint.as_str(),
                submitted = set.len(),
                unique = deduped.len(),
                duplicates,
                "duplicate instruments in the Dhan subscribe set — the same \
                 (security_id, exchange_segment) appeared more than once (I-P1-11). \
                 Subscribing the UNIQUE set; each duplicate would otherwise consume a \
                 wire slot and inflate the planner's count above what the aggregator \
                 actually allocates"
            );
        }
        plan_pool(pool, now, endpoint, &deduped, &mut plan)?;
    }
    Ok(plan)
}

/// Removes repeated `(security_id, exchange_segment)` pairs, preserving order.
///
/// Returns the unique set and how many entries were dropped.
///
/// **`security_id` ALONE is not the key** (I-P1-11): Dhan reuses one numeric id
/// across segments, so `13` is NIFTY on `IDX_I` and a different instrument on
/// `NSE_EQ`. Deduping on the id alone would silently unsubscribe a real
/// instrument — worse than the duplicate it set out to remove.
///
/// Cold path (boot), so the transient `HashSet` is not a hot-path allocation.
/// O(n) average.
#[must_use]
pub fn dedup_subscribe_set(set: &[SubscribeInstrument]) -> (Vec<SubscribeInstrument>, usize) {
    let mut seen: std::collections::HashSet<(SecurityId, ExchangeSegment)> =
        std::collections::HashSet::with_capacity(set.len());
    let mut out = Vec::with_capacity(set.len());
    for inst in set {
        if seen.insert((inst.security_id, inst.segment)) {
            out.push(*inst);
        }
    }
    let duplicates = set.len().saturating_sub(out.len());
    (out, duplicates)
}

/// The number of distinct instruments the fold must allocate a slot for,
/// across ALL THREE pools.
///
/// **Not the sum of the three lengths**, which is what the sizing used to be
/// and which is wrong in both directions at once. The aggregator, the gap
/// detector and the day-OHLC tracker all key on
/// `(Feed, security_id, exchange_segment)` — so a NIFTY option that is
/// subscribed on the main feed AND on depth-20 is **one** slot, not two. The
/// old `main.len() + depth_20.len() + depth_200.len()` counted it twice, which
/// inflated the sizing toward the 25,000 ceiling for instruments that need no
/// extra slot at all.
///
/// Getting this wrong is not a rounding error: `plan_pool` refuses the ENTIRE
/// endpoint when the count does not fit, so an inflated count can take a whole
/// pool dark for capacity the process was never going to use.
#[must_use]
pub fn distinct_fold_slots(
    main_feed: &[SubscribeInstrument],
    depth_20: &[SubscribeInstrument],
    depth_200: &[SubscribeInstrument],
) -> usize {
    let mut seen: std::collections::HashSet<(SecurityId, ExchangeSegment)> =
        std::collections::HashSet::with_capacity(
            main_feed.len() + depth_20.len() + depth_200.len(),
        );
    for set in [main_feed, depth_20, depth_200] {
        for inst in set {
            seen.insert((inst.security_id, inst.segment));
        }
    }
    seen.len()
}

fn plan_pool(
    pool: &mut PoolSupervisor,
    now: std::time::Instant,
    endpoint: DhanEndpointType,
    set: &[SubscribeInstrument],
    plan: &mut FeedStackPlan,
) -> Result<(), FeedStackPlanError> {
    if set.is_empty() {
        return Ok(());
    }
    let cap_per_connection = usize::try_from(endpoint.max_instruments_per_connection())
        .unwrap_or(usize::MAX)
        .max(1);
    let needed = set.len().div_ceil(cap_per_connection);
    let available = endpoint.max_connections();
    if needed > usize::from(available) {
        error!(
            code = ErrorCode::WsGapSubscriptionBatching.code_str(),
            endpoint = endpoint.as_str(),
            instruments = set.len(),
            needed,
            available,
            "refusing to plan the Dhan live feed: this instrument set does not fit the \
             authorized connection count. Widening the pool needs a fresh dated operator \
             quote in websocket-connection-scope-lock.md."
        );
        return Err(FeedStackPlanError::PoolTooSmall {
            endpoint,
            instruments: set.len(),
            needed,
            available,
        });
    }

    // SPREAD across the authorized connections rather than PACK into the
    // fewest (operator directive 2026-08-12, recorded in
    // websocket-connection-scope-lock.md).
    //
    // Packing was `chunks(cap_per_connection)`, which put 4,565 main-feed
    // instruments on ONE socket because Dhan allows 5,000 — leaving four
    // authorized connections idle. Spreading uses the connections the operator
    // paid the authorization for, and is better for three independent reasons:
    //
    //   - failure isolation: one socket dying takes ~1/5 of the book with it,
    //     not all of it;
    //   - head-of-line blocking: one slow frame stalls its own socket's
    //     stream, and a fifth of the universe waits instead of all of it;
    //   - decode parallelism: each connection's read task drains
    //     independently, so frame decode spreads across cores.
    //
    // The Dhan per-connection CAP is still absolute. The shard width is
    // `ceil(len / connections_to_use)`, which is `<= cap_per_connection`
    // whenever `needed <= available` — the condition already enforced above —
    // so spreading can never produce an over-subscribed socket. Pinned by
    // `test_spread_shard_width_never_exceeds_the_dhan_cap`.
    //
    // `connections_to_use` is bounded by the instrument count as well: with 4
    // depth-200 instruments and a 1-per-connection cap, this opens 4 sockets,
    // NOT 5 with an empty one. An empty subscribe is a socket that reports
    // healthy while carrying nothing — the false-OK the scope lock bans.
    // ---- 2026-08-20: main-feed PACKS, depth keeps SPREADING ----
    //
    // The spread directive above was written when the main feed carried ONLY
    // the ~4,565 spot universe and four authorized sockets sat idle. Spreading
    // used what the operator had paid for, and it was right for that shape.
    //
    // The main feed is now dialed in TWO passes: spots at boot, and the
    // ~20,000 option/future contracts once post-open prices exist. Under
    // spread, pass 1 takes `min(5, 4565)` = ALL FIVE connections, so pass 2
    // gets a stateful refusal from `pool.admit` — and because `MainFeed` is
    // the first endpoint in `build_feed_stack_plan`'s loop, that refusal
    // aborted Depth20 and Depth200 planning too. Spreading the small first
    // pass is what starved the sockets the spread directive existed to fill.
    //
    // Packing pass 1 is what actually delivers the directive's intent:
    //
    // | pass | packed | spread |
    // |---|---|---|
    // | boot spots (4,565) | 1 conn | 5 conns |
    // | contracts (~20,000) | 4 conns | 0 — REFUSED |
    // | **main-feed sockets carrying data** | **5** | **1** |
    //
    // At the 25,000 target the two policies converge exactly
    // (`ceil(25000/5000)` = 5 = `min(5, 25000)`), so this changes nothing at
    // full scale and everything today.
    //
    // Depth keeps spreading, and must: depth-200 admits ONE instrument per
    // connection, so packing it would open a single socket and strand four.
    let connections_to_use = if matches!(endpoint, DhanEndpointType::MainFeed) {
        set.len()
            .div_ceil(cap_per_connection.max(1))
            .max(1)
            .min(usize::from(available))
    } else {
        usize::from(available).min(set.len()).max(1)
    };
    let shard_width = set.len().div_ceil(connections_to_use).max(1);
    debug_assert!(
        shard_width <= cap_per_connection,
        "spread shard width {shard_width} exceeds the Dhan per-connection cap \
         {cap_per_connection} for {endpoint:?}"
    );

    for shard in set.chunks(shard_width) {
        let guard = SubscribeGuard::try_new(endpoint, shard.to_vec())?;
        let slot = pool
            .admit(endpoint, now)
            .map_err(|_| FeedStackPlanError::BudgetRefused { endpoint })?;
        plan.connections.push(PlannedConnection { slot, guard });
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// capture_seq — ONE source, and why
// ---------------------------------------------------------------------------

/// Counter: live ticks refused because their frame sequence would not narrow
/// onto the `i64` `capture_seq` column. Fail-closed, never a silent stamp.
pub const INGEST_SEQ_REFUSED_COUNTER: &str = "tv_dhan_feed_ingest_seq_refused_total";

/// Counter: live ticks folded end-to-end (gap detector → aggregator → writer).
pub const INGEST_TICKS_COUNTER: &str = "tv_dhan_feed_ingest_ticks_total";

/// Counter: ticks the aggregator refused, labelled by reason.
pub const INGEST_REFUSED_COUNTER: &str = "tv_dhan_feed_ingest_refused_total";

/// Narrows a WAL frame sequence onto the `i64` `ticks.capture_seq` column.
///
/// # Why this function exists at all — the two-atomic hazard
/// This process contains **two** independent monotonic sequence generators,
/// both seeded `max(prev + 1, wall_clock_nanos)`:
///
/// | Generator | Home | Width |
/// |---|---|---|
/// | [`tickvault_storage::ws_frame_spill::next_frame_seq`] | `WAL_FRAME_SEQ` | `u64` |
/// | [`tickvault_storage::tick_persistence::next_capture_seq`] | `TICK_CAPTURE_SEQ` | `i64` |
///
/// Each is *individually* proven strictly monotonic. Neither is proven
/// monotonic **with respect to the other**, and nothing forces them apart:
/// two CAS loops over two separate atomics, each independently seeded from the
/// same wall clock, will mint the *same* integer whenever they are first
/// touched inside the same nanosecond — which is exactly what happens at
/// boot, when the WAL replay path and a fresh live tick race.
///
/// `capture_seq` is the intra-second tiebreaker in the live DEDUP key
/// (`ts, security_id, segment, capture_seq, feed`). Dhan's `exchange_timestamp`
/// is **second-granular**, so every tick an instrument produces inside one
/// wall-clock second shares `ts`. If two real ticks are ever stamped with the
/// same `capture_seq`, QuestDB UPSERTs one on top of the other and it is gone
/// — silently, with no error, no counter, and no log line. That is the exact
/// zero-loss failure this whole key exists to prevent.
///
/// # The decision: the live path uses the FRAME sequence, and only that
/// [`tickvault_storage::ws_frame_spill`]'s own module docs state the contract
/// verbatim: the frame sequence is minted **once per received frame** and
/// "passes the value to BOTH `WsFrameSpill::append_with_seq` and the live
/// broadcast, so the WAL record and the `ticks.capture_seq` column carry the
/// identical replay-stable value."
///
/// So the frame sequence is the designed source, and it is the one that makes
/// replay idempotent: re-injecting a recovered frame reproduces the SAME
/// `capture_seq`, so the replayed row collapses onto the original instead of
/// duplicating it. A freshly-minted `next_capture_seq()` could not do that —
/// it would mint a *new* value for the same frame and write a duplicate row.
///
/// [`LiveIngest`] therefore calls
/// [`TickWriter::append_tick_with_seq`] exclusively and **never**
/// [`TickWriter::append_tick`], whose convenience body mints from the *other*
/// atomic. `append_tick` stays valid for callers with no frame behind them
/// (synthetic rows, tests); it is simply not reachable from this lane, and
/// `dhan_feed_ingest_never_calls_bare_append_tick` fails the build if that
/// changes.
///
/// # Narrowing
/// Both counters are wall-clock-nanosecond seeded, so a `u64` frame sequence
/// exceeds `i64::MAX` only past the year 2262. Rather than saturate — which
/// would pin every subsequent tick to `i64::MAX` and collapse them all — an
/// unrepresentable value is **refused**, counted, and the tick is dropped
/// loudly. Fail-closed beats a silently-colliding stamp.
#[must_use]
pub fn capture_seq_from_frame_seq(frame_seq: u64) -> Option<i64> {
    i64::try_from(frame_seq).ok()
}

// ---------------------------------------------------------------------------
// Live ingest — the tick→timeframe→disk fold
// ---------------------------------------------------------------------------

/// What one tick did on its way through the fold. Every refusal is a distinct
/// variant so nothing is reported as folded that was not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IngestOutcome {
    /// Folded: observed by the gap detector, folded by the aggregator, and
    /// appended to the tick writer's buffer.
    Folded {
        /// Timeframes that sealed a bucket on this tick.
        sealed: u8,
        /// Timeframes that amended an already-sealed bucket.
        amended: u8,
    },
    /// The frame sequence would not narrow onto `capture_seq` (year-2262
    /// class). Nothing was folded, nothing was written.
    SeqUnrepresentable,
    /// The aggregator refused the tick (insane price, garbage timestamp, or
    /// slot table exhausted). Nothing was folded and nothing was written —
    /// each of those three means the tick itself is unusable.
    AggregatorRefused,
    /// The tick fell OUTSIDE the candle session, so no bucket could open — but
    /// the tick itself is perfectly valid and IS written to `ticks`.
    ///
    /// 2026-08-14. Until now this shared the `AggregatorRefused` exit, so a
    /// tick arriving in the 09:00–09:15 pre-open was discarded entirely:
    /// no candle (correct) AND no raw row (wrong). The session window is a
    /// CANDLE rule — it answers "which bucket does this belong to?" — and
    /// applying it to raw capture threw away the pre-open book-building
    /// window, on a system whose stated requirement is not missing a single
    /// tick. The other three refusals are genuinely unusable data: a NaN or
    /// non-positive price, a timestamp outside a 30-year plausibility band,
    /// or a slot table that cannot identify the instrument consistently.
    /// Those still write nothing.
    WrittenOutOfSession,
    /// The tick was folded but the ILP append failed. Counted as a real loss.
    WriteFailed,
}

/// The live tick fold: gap detector → aggregator → tick writer, in that order,
/// over ONE tick with ONE sequence number.
///
/// This is the seam the transport calls. It owns the three components that
/// survived the 2026-07-17 deletions with zero production callers, and wires
/// them into a single ordered path so they cannot be half-wired.
///
/// # Order is load-bearing
/// The gap detector observes **first**, and unconditionally: an instrument
/// whose price is insane or whose bucket is out of session is exactly the
/// instrument whose silence matters most, and letting the aggregator's refusal
/// suppress the observation would blind the detector to the failure it exists
/// to report.
///
/// # Complexity
/// O(1) per tick: one hash lookup in the detector, one hash lookup plus
/// `TF_COUNT` scalar folds in the aggregator, one ILP row append. No heap
/// allocation in steady state.
pub struct LiveIngest {
    /// Optional sink for the 5 depth levels that ride INLINE in every
    /// Full-mode tick packet (2026-08-19).
    ///
    /// `None` unless a caller opts in via [`LiveIngest::with_inline_depth`].
    ///
    /// CORRECTED 2026-08-20: this said "`None` unless `[dhan_feed]
    /// persist_full_mode_depth` is enabled, and default-OFF deliberately".
    /// **No such config key exists** — not in `config.rs`, not in
    /// `base.toml` — and the production boot site wires this ON
    /// UNCONDITIONALLY, by a dated operator decision recorded at that site
    /// (2026-08-19, all three depth sources kept rather than swapped). So the
    /// comment named a setting a reader could not find and asserted the
    /// opposite of what the lane does; anyone auditing "is inline depth on?"
    /// from this line would have answered no while it wrote ~44 GB/day at
    /// target. The `Option` is real and the default here is still `None` —
    /// that part was always true, and it is what keeps every test and every
    /// other caller unaffected.
    inline_depth: Option<DepthIngest>,
    detector: TickGapDetector,
    /// Edge latch for the dead-class detector: one bit per segment, set while
    /// that class is reported dead.
    ///
    /// An `AtomicU8` rather than a plain `[bool; 8]` because the sweep runs
    /// behind `&self` — and a bitmask because eight classes fit in one byte,
    /// so the whole latch is a single relaxed load. Edge-latched, not
    /// level-triggered: the sweep runs every 30 seconds and a dead class stays
    /// dead all session, so a level trigger would emit ~1,100 identical lines
    /// per class per session. One line per episode is the signal; the rest is
    /// noise that buries it.
    dead_class_latch: AtomicU8,
    aggregator: MultiTfAggregator,
    writer: TickWriter,
    seq_refused: u64,
    /// Ticks the AGGREGATOR refused, cumulative, by reason.
    ///
    /// These exist because the refusal arm below cannot log. It sits on the
    /// per-tick path, and the honest options there are a log line that floods
    /// under a bad-data burst, or nothing. Both are bad. So the arm stays
    /// silent and cheap, the counts accumulate here, and the 30s drain timer
    /// reports the DELTA — the same shape the mark-forward drop uses, for the
    /// same reason.
    ///
    /// Before this, `tv_dhan_feed_ingest_refused_total` incremented and
    /// reached nobody: not the EMF allowlist, not a log. A tick refused for a
    /// bad price or an exhausted slot vanished, and the lane reported healthy.
    refused_price: u64,
    refused_timestamp: u64,
    refused_slot: u64,
    refused_out_of_session: u64,
    seals_emitted: u64,
    seals_dropped: u64,
    /// Bars the fold produced for a timeframe nobody asked for.
    ///
    /// Its own bucket rather than a share of `seals_dropped`, because the two
    /// mean opposite things: `dropped` is data we wanted and lost and should
    /// page someone; this is data we deliberately never wanted. Folding them
    /// together would bury a real loss inside a large, permanently-growing,
    /// entirely benign number.
    seals_skipped: u64,
    /// Sealed candles the writer channel refused that were RESCUED to disk
    /// (spill or DLQ) instead of discarded. Added 2026-08-19 with the no-drop
    /// policy — see [`SEALS_RESCUED_COUNTER`] for why this is not a `dropped`
    /// label.
    seals_rescued: u64,
    /// Rows appended to the ILP buffer since the last flush. The buffer is a
    /// staging area, NOT storage: without a flush the rows never leave the
    /// process, so this counter is what makes the flush happen at all.
    pending_rows: u64,
    /// True once [`Self::spawn_offload_writer`] has moved the blocking ILP
    /// round trip onto a thread of its own.
    ///
    /// Read by `flush_and_record`, which must NOT record feed health from an
    /// offloaded flush: the number that comes back is rows HANDED OFF, and
    /// health is defined as rows LANDED. The writer thread reports it instead.
    writer_offloaded: bool,
    /// The writer thread's join handle, held HERE rather than at the boot site
    /// so the drain — which owns the tail flush — can wait for the last batch
    /// to land.
    ///
    /// This is not tidiness. The shutdown tail exists because "the tail of the
    /// session is exactly the data a naive shutdown loses"; offloading it
    /// without a join would have re-created that loss one queue further out,
    /// with the batch dying in a detached thread as the process exits.
    writer_thread: Option<std::thread::JoinHandle<()>>,
    /// Signalled by the writer thread as its LAST act, so shutdown can wait
    /// with a bounded grace instead of a `join` that has no timeout.
    ///
    /// `JoinHandle::join` blocks forever by contract. A writer wedged on a
    /// hung socket would therefore hang the whole shutdown — trading a lost
    /// tail batch for a box that never stops, which is the worse failure on a
    /// host whose auto-stop is a cost control.
    writer_done: Option<std::sync::mpsc::Receiver<()>>,
}

impl LiveIngest {
    /// Enables persistence of the 5 depth levels that ride inline in every
    /// Full-mode tick packet.
    ///
    /// Builder rather than a constructor argument so every existing call site
    /// keeps its current behaviour by construction — a change that silently
    /// switched depth persistence on for all callers would be exactly the
    /// wrong default for a path that writes hundreds of millions of rows a day.
    #[must_use]
    pub fn with_inline_depth(mut self, sink: DepthIngest) -> Self {
        self.inline_depth = Some(sink);
        self
    }

    /// Sizes the silence detector's slot table independently of the fold's
    /// pre-size.
    ///
    /// # The blind spot this exists to close (MEASURED live, 2026-08-25)
    ///
    /// `new`'s `capacity` is a SOFT pre-size for the fold and a HARD cap for
    /// the detector — `TickGapDetector::with_capacity` never grows and never
    /// reallocates. That asymmetry is invisible at the call site, and it is
    /// the whole defect: the boot site computes `capacity` from the
    /// main-feed set as it stands BEFORE any socket opens, which is the SPOT
    /// universe. The ~22,000 contracts arrive minutes later, through
    /// `run_contract_attach`. Live that day:
    ///
    /// ```text
    /// 08:31:09  refused: 1,276,658  tracked: 865
    /// 12:37:47  refused: 1,211,764  tracked: 865
    /// ```
    ///
    /// 865 is exactly the spot universe. Every contract tick was refused a
    /// slot, so `scan_silence`'s `silent` and `never_ticked` counts described
    /// 865 instruments while reading as though they described all ~23,000 —
    /// the detector's own edge-latched error says precisely this, and it fired
    /// every session. A contract that was silently never subscribed could not
    /// be reported by anything.
    ///
    /// Sizing at the authorized ceiling rather than at a boot-time count is
    /// the point: the ceiling does not depend on WHEN the universe is counted,
    /// so this cannot silently re-break the next time instruments are added
    /// after boot. Cost is ~2 MB of slots plus its index, against a 32 GiB
    /// host.
    ///
    /// Must be called during construction, before any [`Self::seed`] — it
    /// REPLACES the detector, discarding whatever it had learned.
    #[must_use]
    pub fn with_detector_capacity(mut self, capacity: usize) -> Self {
        self.detector = TickGapDetector::with_capacity(capacity.max(1), DetectorConfig::default());
        self
    }

    /// Builds the fold, pre-sized for `capacity` instruments so the slot table
    /// and the detector index never realloc mid-session.
    #[must_use]
    pub fn new(writer: TickWriter, capacity: usize) -> Self {
        Self {
            // OFF unless explicitly enabled — see `with_inline_depth`.
            inline_depth: None,
            detector: TickGapDetector::with_capacity(capacity, DetectorConfig::default()),
            dead_class_latch: AtomicU8::new(0),
            aggregator: MultiTfAggregator::with_capacity(FeedStrategy::DEFAULT, capacity),
            writer,
            seq_refused: 0,
            refused_price: 0,
            refused_timestamp: 0,
            refused_slot: 0,
            refused_out_of_session: 0,
            seals_emitted: 0,
            seals_skipped: 0,
            seals_rescued: 0,
            seals_dropped: 0,
            pending_rows: 0,
            writer_offloaded: false,
            writer_thread: None,
            writer_done: None,
        }
    }

    /// Cumulative aggregator refusals by reason: (price, timestamp, slot,
    /// out-of-session).
    ///
    /// `out_of_session` is returned but is NOT a defect: ticks arriving
    /// outside the fold window are refused by design. It is reported
    /// separately from the other three precisely so a caller cannot lump a
    /// normal pre-open tick in with a bad price and page on it.
    #[must_use]
    pub const fn refusals(&self) -> (u64, u64, u64, u64) {
        (
            self.refused_price,
            self.refused_timestamp,
            self.refused_slot,
            self.refused_out_of_session,
        )
    }

    /// Flushes the ILP buffer to QuestDB. Call on a size OR time trigger —
    /// never per tick (a per-row round-trip would cap throughput at the
    /// network RTT) and never "eventually" (rows that are not flushed are
    /// rows that do not exist, however green the counters look).
    ///
    /// Returns the number of rows the flush covered. A failed flush DISCARDS
    /// the buffer by `TickWriter` contract — loudly, so the loss is counted
    /// rather than silently re-sent forever.
    /// Moves the blocking ILP round trip onto a dedicated OS thread.
    ///
    /// # The coupling this removes
    ///
    /// `TickWriter::flush` is a synchronous HTTP call with a 5 s timeout, and
    /// it ran ON the frame-drain task. `blocking_flush` wrapped it in
    /// `block_in_place`, which is a real mitigation for the RUNTIME — the
    /// other tasks keep their workers — but it does nothing for the drain
    /// itself, and the drain is the only thing emptying the socket. So a slow
    /// QuestDB stopped the fold; the receive buffer filled; and Dhan, whose
    /// published behaviour is to skip a slow consumer forward to "the latest
    /// available state" with no sequence number, discarded the intermediate
    /// ticks at their side. The loss was therefore invisible to every counter
    /// we own, and no amount of provisioned disk throughput removes it,
    /// because the coupling is structural.
    ///
    /// After this call the drain's flush is a bounded-queue hand-off: it
    /// never waits on the network, and when the queue is full it keeps the
    /// rows and retries rather than blocking or dropping.
    ///
    /// # An OS thread, not `spawn_blocking`
    ///
    /// The writer runs for the life of the process doing a blocking wait.
    /// Parking a tokio blocking-pool thread on it forever is exactly what
    /// that pool is not for, and a named OS thread shows up in `top` as
    /// `tv-tick-writer`, which is worth something at 3 a.m.
    ///
    /// # Health is reported HERE
    ///
    /// The thread — not the drain — calls `record_ticks`, with rows that
    /// actually LANDED. Reporting on hand-off instead would forge liveness
    /// during a database outage: the queue would accept batches happily while
    /// nothing reached the database, and `feed_health` would read green for
    /// precisely as long as the data was going nowhere.
    pub fn spawn_offload_writer(
        &mut self,
        feed_health: Arc<tickvault_common::feed_health::FeedHealthRegistry>,
    ) -> std::io::Result<()> {
        let placeholder = TickWriter::for_test(Feed::Dhan);
        let live = std::mem::replace(&mut self.writer, placeholder);
        let (producer, mut sink, rx) = live.split_for_offload();
        self.writer = producer;
        self.writer_offloaded = true;
        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        let handle = std::thread::Builder::new()
            .name("tv-tick-writer".to_owned())
            .spawn(move || {
                // `recv` ends only when every sender is dropped, i.e. when the
                // drain itself is gone. There is no other exit, deliberately:
                // a writer that could stop on its own would leave the producer
                // handing rows to a closed queue.
                while let Ok(mut batch) = rx.recv() {
                    let landed = sink.write(&mut batch);
                    report_tick_persistence(landed > 0);
                    feed_health.record_ticks(
                        Feed::Dhan,
                        landed as u64,
                        chrono::Utc::now()
                            .timestamp_nanos_opt()
                            .unwrap_or(0)
                            .saturating_add(tickvault_common::constants::IST_UTC_OFFSET_NANOS),
                    );
                }
                info!("tick writer thread exiting — the drain closed its queue");
                // Last act: tell shutdown the queue is fully drained.
                //
                // The result is HANDLED, not discarded, and the crate leaves no
                // third option — `lib.rs` denies `unused_must_use` AND
                // `clippy::let_underscore_must_use`, so both `drop(...)` and
                // `let _ = ...` are rejected here by design. That is the right
                // rule: a failed send is not nothing, it means the receiver
                // gave up first.
                //
                // Which is worth a line, because it is the one case where the
                // shutdown's own error over-reports: it will already have said
                // the writer "did not finish", and this says it did — just too
                // late. Debug rather than warn, since by then the operator has
                // the louder message and the spill path has the rows.
                if done_tx.send(()).is_err() {
                    tracing::debug!(
                        "tick writer finished AFTER the shutdown grace expired — \
                         the timeout already reported; the rows are accounted for"
                    );
                }
            })?;
        self.writer_thread = Some(handle);
        self.writer_done = Some(done_rx);
        Ok(())
    }

    /// Closes the hand-off queue and WAITS for the writer thread to finish.
    ///
    /// Call once, after the shutdown tail flush. The order is load-bearing:
    /// the tail flush hands the last batch to the queue, closing the queue
    /// tells the writer there is no more, and the join is what guarantees that
    /// batch reaches QuestDB (or the spill tier) before the process goes away.
    /// Skipping the join loses exactly the rows the tail flush exists to save.
    ///
    /// Idempotent and safe on a lane that was never offloaded.
    pub fn shutdown_offload_writer(&mut self) {
        self.writer.close_offload();
        self.writer_offloaded = false;
        let Some(handle) = self.writer_thread.take() else {
            return;
        };
        // BOUNDED, not `join()`. The grace is generous against the ILP client's
        // own 5 s request timeout — one in-flight flush plus the queue behind
        // it — but it is finite, because a writer wedged on a hung socket must
        // not be able to hang the box's shutdown.
        let finished = self
            .writer_done
            .take()
            .is_some_and(|rx| rx.recv_timeout(OFFLOAD_SHUTDOWN_GRACE).is_ok());
        if !finished {
            error!(
                code = ErrorCode::HotPath02WriterQueueDrop.code_str(),
                grace_secs = OFFLOAD_SHUTDOWN_GRACE.as_secs(),
                "the tick writer did not finish within the shutdown grace — the \
                 final batch of the session may not have reached QuestDB. Check \
                 the tick spill directory; the rows are re-ingestable if they \
                 were rescued."
            );
            // Deliberately NOT joined after a timeout: joining here is exactly
            // the unbounded wait the grace exists to avoid. The thread is
            // detached and the process is going away.
            return;
        }
        if handle.join().is_err() {
            error!(
                code = ErrorCode::HotPath02WriterQueueDrop.code_str(),
                "the tick writer thread PANICKED — the final batch of the session \
                 may not have reached QuestDB. Check the tick spill directory."
            );
        }
    }

    /// Has the blocking flush been moved off the drain task?
    #[must_use]
    // TEST-EXEMPT: accessor, exercised by the offload wiring tests below.
    pub const fn writer_is_offloaded(&self) -> bool {
        self.writer_offloaded
    }

    pub fn flush(&mut self) -> u64 {
        // Flush the inline-depth sink FIRST, and unconditionally.
        //
        // This sits above the `pending_rows == 0` early return deliberately.
        // Depth rows and tick rows are appended on different conditions: a
        // Full-mode packet whose tick the aggregator refuses still contributed
        // depth rows, so `pending_rows` can be 0 while the depth buffer is not
        // empty. Returning early would leave those rows appended-but-never-
        // flushed — and by this function's own docstring, rows that are not
        // flushed are rows that do not exist, however green the counters look.
        //
        // Failure is counted and logged, never propagated: the tick flush
        // below is the lane's primary durability path and must not be skipped
        // because the depth writer had a bad moment.
        if let Some(sink) = self.inline_depth.as_mut()
            && let Err(err) = sink.flush()
        {
            counters().depth_flush_failed.increment(1);
            error!(
                code = "TICK-FLUSH-01",
                error = %format!("{err:#}"),
                "inline-depth flush FAILED — those depth rows are lost (the \
                 ILP buffer is discarded on a failed flush). Tick persistence \
                 is unaffected."
            );
        }
        if self.pending_rows == 0 {
            return 0;
        }
        let covered = self.pending_rows;
        self.pending_rows = 0;
        match self.writer.flush() {
            Ok(()) => {
                counters().flush_ok.increment(1);
                report_tick_persistence(true);
                covered
            }
            Err(err) => {
                counters().flush_failed.increment(1);
                report_tick_persistence(false);
                // 2026-08-12 — the second sentence of this message used to
                // read "The raw frames remain in the write-ahead log and are
                // recoverable by replay." That was wrong in a way that
                // mattered: an operator reading it would treat a flush
                // failure as deferred rather than as loss.
                //
                // CORRECTED 2026-08-19. This block used to say replay "logs
                // the replayed live-feed frames and DROPS them — there is no
                // re-fold path (the fold takes a live ring, not a replay
                // batch)". That was true when written and became FALSE on
                // 2026-08-15, when `refold_wal_frames` landed — it takes
                // exactly the replay batch the comment said could not exist,
                // and `main.rs` hands the staged frames to the lane whenever
                // the lane will run.
                //
                // The correction is not cosmetic: a same-day hostile audit
                // read this comment, trusted it over the code, and reported
                // the WAL as write-only — a false CRITICAL. A stale comment
                // does not merely fail to inform, it manufactures findings,
                // which is the exact failure this repo has hit before.
                //
                // What is TRUE today, both halves stated:
                //   ACROSS A RESTART — recovered. The next boot re-folds
                //   these frames, DEDUP-idempotently (`capture_seq` is read
                //   back from the WAL record, never re-stamped).
                //   INTRA-SESSION — NOT recovered. Nothing re-reads the WAL
                //   while the process lives, so until a restart these rows
                //   are absent from QuestDB.
                // So a flush failure is deferred loss if the process later
                // restarts, and standing loss if it does not.
                //
                // AMENDED 2026-08-21. The INTRA-SESSION half above is no
                // longer true, and this is the third dated correction to this
                // one block — which is itself the point the block makes.
                // `TickWriter::discard_pending` now rescues the failed batch
                // to a tick spill file holding the ILP verbatim, so the rows
                // are replayable WITHOUT a restart by one curl against
                // QuestDB's /write, idempotently (the dedup key carries
                // `capture_seq`). So there is no longer a "standing loss"
                // case while the process lives: the honest statement is that
                // the rows are absent from the database until someone replays
                // the file or the box reboots, and BOTH recoveries exist.
                // The `error!` below was rewritten in the same change — it
                // still said recovery was impossible, which contradicted the
                // WS-GAP-03 line printed beside it for the same event.
                error!(
                    code = ErrorCode::WsGapConnectionState.code_str(),
                    %err,
                    rows = covered,
                    "live tick flush to QuestDB FAILED — these rows are NOT in the database. \
                     They are NOT lost, and there are TWO recoveries. Immediately: the writer \
                     rescues the failed batch to a tick spill file (the WS-GAP-03 line beside \
                     this one names the exact path) and it re-ingests with a single curl, \
                     safely repeatable. Failing that, the raw frames are in the write-ahead \
                     log and the next boot re-folds them idempotently, \
                     so a restart recovers this window. \
                     Fix the database first, then replay the spill file."

                );
                0
            }
        }
    }

    /// Rows appended but not yet flushed.
    #[must_use]
    pub const fn pending_rows(&self) -> u64 {
        self.pending_rows
    }

    /// Registers an instrument before any tick arrives, so a stream that never
    /// delivers a single tick is still reported as silent rather than being
    /// invisible. Returns `false` when detector capacity is exhausted.
    pub fn seed(&mut self, security_id: u64, segment: ExchangeSegment, now_millis: u64) -> bool {
        self.detector.seed((security_id, segment), now_millis)
    }

    /// Folds one tick. `frame_seq` MUST be the sequence minted for this tick's
    /// frame by [`tickvault_storage::ws_frame_spill::next_frame_seq`] — see
    /// [`capture_seq_from_frame_seq`] for why nothing else is acceptable.
    pub fn ingest_tick(
        &mut self,
        tick: &ParsedTick,
        frame_seq: u64,
        recv_monotonic_millis: u64,
    ) -> IngestOutcome {
        self.ingest_tick_at(tick, frame_seq, 0, recv_monotonic_millis)
    }

    /// Folds the `packet_index`-th tick parsed out of one frame.
    ///
    /// # Why the index matters
    /// One WebSocket message can carry several packets, but the WAL mints ONE
    /// sequence per FRAME. The `ticks` DEDUP key is
    /// `(ts, security_id, segment, capture_seq, feed)`. Two packets in the same
    /// frame for the same instrument at the same exchange timestamp — two
    /// trades inside one second, ordinary for a liquid instrument — would share
    /// every key column, so QuestDB would upsert the second onto the first and
    /// the tick would vanish while the ingest counter still called it folded.
    /// Silent loss with a green light: exactly the class this lane exists to
    /// avoid.
    ///
    /// # The chosen trade-off, stated plainly
    /// Packet 0 uses the frame's own sequence, so the overwhelmingly common
    /// single-packet frame keeps the documented replay-stable property: a
    /// replayed frame reproduces the identical `capture_seq` and collapses onto
    /// the original row instead of duplicating it.
    ///
    /// Packets 1..N derive theirs from the SAME frame sequence by OR-ing the
    /// packet index into reserved low bits
    /// (`ws_frame_spill::packet_capture_seq`), so every packet of a replayed
    /// frame reproduces its original `capture_seq` exactly and collapses onto
    /// the original row.
    ///
    /// ## What this replaced, and why the old reasoning was wrong (2026-08-14)
    ///
    /// Until now packets 1..N minted a FRESH sequence from the process-wide
    /// counter. Those values are globally unique, but they are NOT in the WAL
    /// and cannot be regenerated — so a replay that re-folded such a frame
    /// would write DUPLICATE rows for its 2nd..Nth packets. The comment here
    /// argued that was "the right way round" because a duplicate row is
    /// visible and a dropped tick is not.
    ///
    /// That trade was real but it was blocking the wrong thing: it made WAL
    /// re-fold — the single largest CONTROLLABLE tick-loss path in the system —
    /// unsafe to build. The premise it rested on was also wrong. It said
    /// "there is no headroom to carve a packet index into", having considered
    /// only MULTIPLYING the sequence (`frame_seq * MAX_PACKETS_PER_FRAME`),
    /// which is indeed impossible: ≈1.786e18 × 70,000 ≈ 1.25e23 overflows
    /// `i64::MAX` by four orders of magnitude and would refuse every tick.
    ///
    /// Reserving low bits in the SEED costs no headroom at all —
    /// `(n >> 17) << 17` only clears bits, leaving the magnitude and the ≈5.16×
    /// margin to `i64::MAX` unchanged. Uniqueness is structural: every frame
    /// base ends in 17 zero bits and the base is strictly increasing, so one
    /// frame's 131,072 packet slots cannot reach the next frame's base. The
    /// old comment's fear — trading a visible duplicate for an invisible
    /// collision — does not apply to a scheme where collisions are impossible
    /// by construction rather than unlikely by argument.
    pub fn ingest_tick_at(
        &mut self,
        tick: &ParsedTick,
        frame_seq: u64,
        packet_index: u32,
        recv_monotonic_millis: u64,
    ) -> IngestOutcome {
        // REFUSE rather than fall back to a fresh sequence when the index does
        // not fit: a fresh sequence is precisely the un-regenerable value this
        // scheme exists to eliminate, so falling back would quietly restore the
        // duplicate-on-replay defect while looking like it had worked.
        let frame_seq = match tickvault_storage::ws_frame_spill::packet_capture_seq(
            frame_seq,
            u64::from(packet_index),
        ) {
            Some(seq) => seq,
            None => {
                self.seq_refused = self.seq_refused.saturating_add(1);
                counters().ingest_seq_refused.increment(1);
                error!(
                    code = ErrorCode::WsGapConnectionState.code_str(),
                    frame_seq,
                    packet_index,
                    security_id = tick.security_id,
                    "live tick refused: packet index exceeds the bits reserved in \
                     capture_seq. The tick was NOT folded and NOT written — a counted \
                     loss, never a fresh sequence that would duplicate on WAL replay."
                );
                return IngestOutcome::SeqUnrepresentable;
            }
        };
        // Sequence FIRST: if we cannot stamp this row safely we must not touch
        // any fold state, or the aggregator would carry a tick that never
        // reached disk.
        let Some(capture_seq) = capture_seq_from_frame_seq(frame_seq) else {
            self.seq_refused = self.seq_refused.saturating_add(1);
            counters().ingest_seq_refused.increment(1);
            error!(
                code = ErrorCode::WsGapConnectionState.code_str(),
                frame_seq,
                security_id = tick.security_id,
                "live tick refused: frame sequence does not fit the capture_seq column. \
                 The tick was NOT folded and NOT written — this is a real, counted loss, \
                 never a silent stamp that would collapse two rows under the DEDUP key."
            );
            return IngestOutcome::SeqUnrepresentable;
        };

        // The gap detector observes BEFORE the fold, and deliberately so.
        //
        // (Until 2026-08-25 this comment said "see the type docs on order".
        // Those docs say nothing about ordering — they cover segment mapping.
        // The pointer was to a justification that did not exist, so here is
        // the real one.)
        //
        // `observe` answers "is the feed still delivering PACKETS for this
        // instrument", not "is this instrument producing usable data". A tick
        // that arrives and is then refused by the aggregator — a poisoned
        // timestamp, a non-finite price — is still proof the socket is alive
        // for that security. Moving the call below the refusal would silently
        // change the question, and would re-open the crying-wolf class this
        // module documents at `SilenceVerdict::Warming`: a legitimately sparse
        // contract would then page every session open.
        //
        // KNOWN RESIDUAL, recorded rather than papered over: an instrument
        // whose ticks ALL arrive and are ALL refused therefore reads healthy
        // to the silence detector while producing nothing. That is a real
        // unmonitored state. It is NOT fixed by reordering — it needs its own
        // signal, and one that stays O(1) in space: a per-instrument refusal
        // map is exactly the unbounded-growth shape this codebase keeps
        // finding and removing. The refusal counters (`refused_price`,
        // `refused_timestamp`, `refused_slot_exhausted`) already carry the
        // aggregate, and AGGREGATOR-DROP-01's 30-second delta report is where
        // a systemic refusal rate surfaces today.
        if let Some(obs) = TickObservation::from_parsed_tick(tick, recv_monotonic_millis) {
            let _assessment = self.detector.observe(obs);
        }

        // Seals go STRAIGHT to the process-wide seal writer — the same
        // `global_seal_sender` the REST fold uses, landing in the same
        // `candles_<tf>` tables under the same DEDUP key. An earlier draft of
        // this fold kept its own private `SealRing` that nothing drained: every
        // sealed candle was computed and then thrown away, which is the entire
        // point of the lane silently producing nothing. A ring is a buffer in
        // front of a writer; without the writer it is a shredder.
        let mut emitted = 0u64;
        let mut dropped = 0u64;
        let mut rescued = 0u64;
        let mut skipped = 0u64;
        let sender = tickvault_storage::seal_writer_runner::global_seal_sender();
        let stats: ConsumeStats = self.aggregator.consume_tick(
            Feed::Dhan,
            tick,
            None,
            |feed, security_id, segment_code, tf, state| {
                // Emit rows ONLY for the thirteen timeframes the operator
                // asked for (Quote 13, 2026-08-08). The enum carries 24, so
                // eleven second-scale frames — S2 S3 S4 S6 S7 S8 S9 S11 S12
                // S13 S14 — were writing a row per bucket for nobody.
                //
                // Counted into its OWN bucket, never into `dropped`: that
                // counter means data we wanted and lost, and conflating
                // "never asked for it" with "lost it" would make every drop
                // alarm permanently noisy while hiding real losses in the
                // noise. It is counted rather than silently returned because
                // `test_seal_open_buckets_at_close_accounts_every_bar_it_produces`
                // pins that no bar escapes accounting on ANY side — a bare
                // `return` here made bars vanish from the ledger entirely,
                // and that test caught it.
                //
                // The fold still computes all 24 slots. Only emission is
                // gated, so ordinals, the `[_; TF_COUNT]` arrays and the
                // audit-table `timeframe` symbols are all untouched.
                // Pinned by `tf_index::tests::tf_index_operator_set_is_twelve`.
                if !tf.is_operator_requested() {
                    skipped = skipped.saturating_add(1);
                    return;
                }
                let seal = BufferedSeal::new(security_id, segment_code, tf, state, feed);
                // No writer channel installed at all. Before 2026-08-19 this
                // discarded the seal outright; it now takes the same durable
                // route a full channel does, so a boot-order problem costs a
                // disk write rather than a day of candles.
                let Some(tx) = sender else {
                    match escalate_refused_seal(&seal) {
                        SealRefusal::Rescued => rescued = rescued.saturating_add(1),
                        SealRefusal::Lost => dropped = dropped.saturating_add(1),
                    }
                    return;
                };
                // `try_send`, never `send().await`: this closure runs inside
                // the per-tick fold, and awaiting here would let a slow seal
                // writer stall tick ingestion.
                // NEVER discard. Operator directive 2026-08-19: "never ever
                // drop any ticks irrespective of any worst case". A refused
                // seal goes to disk (spill, then DLQ); only a seal both disk
                // tiers reject is counted as lost, and that fires
                // AGGREGATOR-DROP-01.
                if let Err(refused) = tx.try_send(seal) {
                    match escalate_refused_seal(&refused.into_inner()) {
                        SealRefusal::Rescued => rescued = rescued.saturating_add(1),
                        SealRefusal::Lost => dropped = dropped.saturating_add(1),
                    }
                } else {
                    emitted = emitted.saturating_add(1);
                }
            },
        );
        self.seals_emitted = self.seals_emitted.saturating_add(emitted);
        self.seals_dropped = self.seals_dropped.saturating_add(dropped);
        self.seals_rescued = self.seals_rescued.saturating_add(rescued);
        self.seals_skipped = self.seals_skipped.saturating_add(skipped);
        if emitted > 0 {
            counters().seals_emitted.increment(emitted);
        }
        if dropped > 0 {
            counters().seals_dropped.increment(dropped);
        }
        if rescued > 0 {
            counters().seals_rescued.increment(rescued);
        }

        // `refused_timestamp` is checked here alongside the other three. It was
        // missing in an earlier draft, so a tick with an implausible exchange
        // timestamp folded into NOTHING and still fell through to the writer,
        // returning `Folded` — a row stamped at a garbage designated timestamp,
        // reported as success.
        // CANDLE-ONLY REFUSALS — the data is fine, only the BUCKET is missing.
        //
        // Two conditions qualify, and both keep the tick row:
        //
        //   `out_of_session`      the pre-open window is real data with no
        //                         bucket to fold into.
        //   `untraded_sentinel`   an exact 0.0 price: the vendor saying "no
        //                         last traded price", which is TRUE. Added
        //                         2026-08-20 — see `ConsumeStats`. Folding a
        //                         zero would corrupt the OHLC, so the candle
        //                         is skipped, but discarding the ROW loses the
        //                         ability to tell "did not trade" from "did
        //                         not capture", and costs the packet's open
        //                         interest and bid/ask with it. On the live
        //                         box that was ~22,000 ticks a session.
        //
        // The three below are different IN KIND — a NaN price, a timestamp
        // outside a 30-year band, or an unidentifiable instrument — and
        // writing any of them would put a corrupt row in `ticks` under a
        // garbage designated timestamp, which is worse than losing it.
        // Three conditions refuse the WHOLE tick, because writing the row
        // would put corrupt data in `ticks` under a garbage designated
        // timestamp: a price outside `[0, MAX]` (NaN and both infinities fall
        // outside by comparison), a timestamp beyond a 30-year band, and an
        // instrument with no fold slot at all.
        let hard_refusal = stats.refused_price || stats.refused_timestamp || stats.slot_exhausted;

        // Two refuse only the CANDLE and keep the row — see above. They are
        // mutually exclusive with the three by construction, which is why this
        // reads as a plain `!hard_refusal` rather than repeating them.
        // `stale_trading_day` joins the candle-only set on 2026-08-26.
        //
        // Dhan sends the LAST TRADE TIME, so a dormant contract snapshotted
        // now carries a timestamp from whenever it last traded (measured mean
        // ~5 hours, max 34 days). Folding that opens a candle bucket on a day
        // that already closed — verified live as 8,898 fabricated bars in a
        // database created empty that same morning — and, worse, leaves a
        // bucket open so today's real 09:15 tick takes the CONTINUE path and
        // the day-open arm never fires.
        //
        // The ROW is kept, for exactly the reason `untraded_sentinel` is kept:
        // the tick is not corrupt. It is a real last-traded price with a real
        // old trade time, carrying live open interest and bid/ask. Discarding
        // it would lose the ability to tell "did not trade today" from "did
        // not capture", which is the same false-OK the 2026-08-20 fix removed.
        let candle_only_refusal = (stats.out_of_session
            || stats.untraded_sentinel
            || stats.stale_trading_day
            || stats.untraded_timestamp)
            && !hard_refusal;

        if hard_refusal {
            let reason = if stats.refused_price {
                self.refused_price = self.refused_price.saturating_add(1);
                "price"
            } else if stats.refused_timestamp {
                self.refused_timestamp = self.refused_timestamp.saturating_add(1);
                "timestamp"
            } else if stats.slot_exhausted {
                self.refused_slot = self.refused_slot.saturating_add(1);
                "slot_exhausted"
            } else {
                self.refused_out_of_session = self.refused_out_of_session.saturating_add(1);
                "out_of_session"
            };
            // No log here, deliberately: this is the per-tick path, and a bad
            // upstream burst would turn a log line into a flood that buries
            // the signal it was meant to raise. The counts above are reported
            // as a delta by the 30s drain timer instead — visible, bounded,
            // and impossible to flood.
            counters().refused(reason).increment(1);
            return IngestOutcome::AggregatorRefused;
        }

        // `append_tick_with_seq`, never `append_tick` — the single-source rule.
        if self.writer.append_tick_with_seq(tick, capture_seq).is_err() {
            error!(
                code = ErrorCode::WsGapConnectionState.code_str(),
                security_id = tick.security_id,
                capture_seq,
                "live tick folded but its ILP append failed — counted as loss"
            );
            return IngestOutcome::WriteFailed;
        }

        // Count the buffered row. This is what makes the size-based flush
        // trigger fire at all — without it the counter stays 0, the threshold
        // is never reached, and rows accumulate in the ILP buffer forever
        // while every metric reports success.
        self.pending_rows = self.pending_rows.saturating_add(1);
        counters().ingest_ticks.increment(1);
        if candle_only_refusal {
            // Counted under the SAME `out_of_session` reason as before, so the
            // existing 30s delta report and any dashboard built on it keep
            // meaning what they meant: "this many ticks opened no candle
            // bucket". What changed is that they are now also rows in `ticks`,
            // which the return value says explicitly rather than leaving the
            // caller to infer it from a counter.
            self.refused_out_of_session = self.refused_out_of_session.saturating_add(1);
            counters().refused("out_of_session").increment(1);
            return IngestOutcome::WrittenOutOfSession;
        }
        IngestOutcome::Folded {
            sealed: stats.sealed_count,
            amended: stats.amended_count,
        }
    }

    /// Seals every OPEN bucket across every instrument and timeframe, routing
    /// each one to the same seal writer the per-tick fold uses.
    ///
    /// # Why this has to exist
    ///
    /// A bucket seals when a LATER tick crosses its boundary. That rule has a
    /// hole at exactly one place — the end — and the end happens every single
    /// day: at 15:30 the final bucket of every timeframe for every instrument
    /// is still open, no further tick will ever arrive to close it, and it is
    /// discarded when the process exits. One bar per instrument per timeframe,
    /// lost daily, with no counter moving and no log line.
    ///
    /// `MultiTfAggregator::force_seal_all` was written for this and had **zero
    /// production callers** — found 2026-08-11 by a workspace complexity audit,
    /// and it is the fourth defect of this exact shape in this lane: code that
    /// exists, is tested, and is never invoked. `seal_ring.rs` even sizes the
    /// ring to absorb the burst this produces, so the buffer was provisioned
    /// for a caller that did not exist.
    ///
    /// Returns `(emitted, dropped)`. Both are added to the running totals, so
    /// the close seal shows up in the same counters as the per-tick path — a
    /// close-time drop is as much a loss as a mid-session one and must not be
    /// accounted separately.
    ///
    /// # Complexity
    /// O(slots × TF). COLD — once per session, never per tick.
    pub fn seal_open_buckets_at_close(&mut self) -> (u64, u64) {
        let mut emitted = 0u64;
        let mut dropped = 0u64;
        let mut rescued = 0u64;
        let mut skipped = 0u64;
        let sender = tickvault_storage::seal_writer_runner::global_seal_sender();
        let bars = self
            .aggregator
            .force_seal_all(|feed, security_id, segment_code, tf, state| {
                // Emit rows ONLY for the thirteen timeframes the operator
                // asked for (Quote 13, 2026-08-08). The enum carries 24, so
                // eleven second-scale frames — S2 S3 S4 S6 S7 S8 S9 S11 S12
                // S13 S14 — were writing a row per bucket for nobody.
                //
                // Counted into its OWN bucket, never into `dropped`: that
                // counter means data we wanted and lost, and conflating
                // "never asked for it" with "lost it" would make every drop
                // alarm permanently noisy while hiding real losses in the
                // noise. It is counted rather than silently returned because
                // `test_seal_open_buckets_at_close_accounts_every_bar_it_produces`
                // pins that no bar escapes accounting on ANY side — a bare
                // `return` here made bars vanish from the ledger entirely,
                // and that test caught it.
                //
                // The fold still computes all 24 slots. Only emission is
                // gated, so ordinals, the `[_; TF_COUNT]` arrays and the
                // audit-table `timeframe` symbols are all untouched.
                // Pinned by `tf_index::tests::tf_index_operator_set_is_twelve`.
                if !tf.is_operator_requested() {
                    skipped = skipped.saturating_add(1);
                    return;
                }
                let seal = BufferedSeal::new(security_id, segment_code, tf, state, feed);
                // No writer channel installed at all. Before 2026-08-19 this
                // discarded the seal outright; it now takes the same durable
                // route a full channel does, so a boot-order problem costs a
                // disk write rather than a day of candles.
                let Some(tx) = sender else {
                    match escalate_refused_seal(&seal) {
                        SealRefusal::Rescued => rescued = rescued.saturating_add(1),
                        SealRefusal::Lost => dropped = dropped.saturating_add(1),
                    }
                    return;
                };
                // `try_send` for the same reason the per-tick path uses it: a
                // slow writer must never block the caller. At close the caller
                // is the drain's own shutdown, and blocking it would hold the
                // whole lane open past the session.
                // NEVER discard. Operator directive 2026-08-19: "never ever
                // drop any ticks irrespective of any worst case". A refused
                // seal goes to disk (spill, then DLQ); only a seal both disk
                // tiers reject is counted as lost, and that fires
                // AGGREGATOR-DROP-01.
                if let Err(refused) = tx.try_send(seal) {
                    match escalate_refused_seal(&refused.into_inner()) {
                        SealRefusal::Rescued => rescued = rescued.saturating_add(1),
                        SealRefusal::Lost => dropped = dropped.saturating_add(1),
                    }
                } else {
                    emitted = emitted.saturating_add(1);
                }
            });
        debug_assert_eq!(
            bars as u64,
            emitted.saturating_add(dropped).saturating_add(skipped),
            "every bar force_seal_all produced must be accounted as emitted, \
             dropped, or skipped-as-unrequested"
        );
        self.seals_emitted = self.seals_emitted.saturating_add(emitted);
        self.seals_dropped = self.seals_dropped.saturating_add(dropped);
        self.seals_rescued = self.seals_rescued.saturating_add(rescued);
        self.seals_skipped = self.seals_skipped.saturating_add(skipped);
        if emitted > 0 {
            counters().seals_emitted.increment(emitted);
        }
        if dropped > 0 {
            counters().seals_dropped.increment(dropped);
        }
        if rescued > 0 {
            counters().seals_rescued.increment(rescued);
        }
        (emitted, dropped)
    }

    /// Asks the gap detector what it has been recording, and reports it.
    ///
    /// # Why this has to exist
    ///
    /// The lane builds a [`TickGapDetector`], seeds every subscribed
    /// instrument into it, and calls `observe` on every tick — and then never
    /// asked it a single question. `scan_silence` had **zero production
    /// callers**: a fully wired sensor with no read-out, which reads greener
    /// than dead code does, because every part of it looks connected.
    ///
    /// Two distinct failures become visible here, and neither is visible any
    /// other way:
    ///
    /// * [`SilenceVerdict::NeverTicked`] — the instrument was subscribed and
    ///   has produced nothing at all. A subscribe that silently did not take
    ///   leaves no payload to reason about, so *absence against a seeded key*
    ///   is the only evidence that exists. This is the partial-subscribe
    ///   detector.
    /// * [`SilenceVerdict::Exceeded`] — the instrument ticked, then went quiet
    ///   for longer than its OWN learned cadence predicts. Judged per
    ///   instrument rather than against one global threshold, so a slow
    ///   contract is not compared to a fast index.
    ///
    /// Sparse instruments (far-month options, the INDIA VIX class) are
    /// reported but never counted toward the alarm — the §36.4 precedent.
    /// Counting them would page for legitimate quiet and the whole read-out
    /// would be turned off within a week.
    ///
    /// Returns `(silent, never_ticked)` where `silent` counts only
    /// alarm-worthy reports.
    ///
    /// # Complexity
    /// O(n) in tracked instruments, allocation-free (the sink is a closure
    /// over stack locals, not a `Vec`). COLD — every
    /// [`SILENCE_SCAN_INTERVAL`], never per tick.
    pub fn scan_silence(&self, now_millis: u64) -> (u64, u64) {
        let mut discard = [SilentInstrument::EMPTY; WORST_SILENT_NAMED];
        let (silent, never, _) = self.scan_silence_named(now_millis, &mut discard);
        (silent, never)
    }

    /// [`Self::scan_silence`], but it also hands back the IDENTITIES of the
    /// quietest instruments.
    ///
    /// # Why a second entry point exists
    ///
    /// Until 2026-08-21 the silence page reported COUNTS only — "47 subscribed
    /// instruments are quiet" — and no `security_id` was written to any log,
    /// metric or table. The single worst offender was captured and then logged
    /// at `debug!`, which does not reach `errors.jsonl` and therefore never
    /// reaches CloudWatch.
    ///
    /// So the question an operator actually asks the morning after — *which*
    /// instruments went silent — was unanswerable by construction. Not hard to
    /// answer, not slow to answer: the information was computed, used to
    /// increment a counter, and discarded. And it is the one failure with no
    /// other evidence anywhere in the system, because a stream that never
    /// arrives leaves nothing to count and nothing to fail to parse.
    ///
    /// # Why a fixed buffer rather than a `Vec`
    ///
    /// The caller owns the storage, so this stays allocation-free on a path
    /// that already sweeps every tracked instrument. `WORST_SILENT_NAMED`
    /// bounds what a single episode can write to the log: at 25,000
    /// instruments an unbounded list would be a 25,000-line burst into the
    /// sink, which buries the very signal it is reporting.
    ///
    /// Returns `(silent, never_ticked, named)` where `named` is how many
    /// entries of `worst` were filled — never more than its length.
    ///
    /// # Complexity
    /// O(n) in tracked instruments — unchanged, and inherent: "which are
    /// silent?" is a question about all of them at once. The ranking adds a
    /// bounded insertion into a `WORST_SILENT_NAMED`-element array per
    /// alarm-worthy report, so it is O(n × K) with K = 8 and no allocation.
    pub fn scan_silence_named(
        &self,
        now_millis: u64,
        worst: &mut [SilentInstrument; WORST_SILENT_NAMED],
    ) -> (u64, u64, usize) {
        let mut silent = 0u64;
        let mut never = 0u64;
        let mut named = 0usize;
        let mut classes = ClassLiveness::default();
        self.detector.scan_silence(now_millis, |report| {
            // CLASS ROLLUP — folded here, BEFORE the alarm filter below, and
            // deliberately not behind `counts_toward_alarm()`.
            //
            // That filter is `!sparse && (Exceeded | NeverTicked)`, so it
            // drops HEALTHY instruments. Reusing it as the class denominator
            // would leave a denominator of "only the troubled ones", making
            // `never == eligible` true the moment any instrument in a segment
            // had never ticked — the detector would fire on a healthy lane
            // every sweep. The rollup therefore does its own classification
            // from the raw report, and rides this sweep rather than adding a
            // second O(n) pass over the universe.
            classes.observe(
                report.key.1,
                report.sparse,
                report.verdict == SilenceVerdict::NeverTicked,
                report.silent_millis > report.expected_millis,
            );
            if !report.counts_toward_alarm() {
                return;
            }
            // `NeverTicked` is returned by `classify_silence` the instant an
            // instrument is seeded, with NO elapsed-time condition — it means
            // "has never ticked", full stop. Counting it raw would report
            // every instrument in the book as silent for the whole window
            // between subscribing and the first tick, which is a false alarm
            // on every single boot. `silent_millis` is time-since-seeding for
            // this verdict, so requiring it to clear the same quiet ceiling
            // the other verdicts are judged against gives the subscribe a
            // fair chance to produce something first.
            //
            // Found by a test, not by review: the first version asserted a
            // freshly-seeded instrument reports nothing and it reported (1,1).
            if report.silent_millis <= report.expected_millis {
                return;
            }
            silent = silent.saturating_add(1);
            if report.verdict == SilenceVerdict::NeverTicked {
                never = never.saturating_add(1);
            }
            named = rank_silent(
                worst,
                named,
                SilentInstrument {
                    security_id: report.key.0,
                    segment: report.key.1,
                    silent_millis: report.silent_millis,
                    expected_millis: report.expected_millis,
                    never_ticked: report.verdict == SilenceVerdict::NeverTicked,
                },
            );
        });
        metrics::gauge!(INSTRUMENTS_SILENT_GAUGE).set(silent as f64);
        metrics::gauge!(INSTRUMENTS_NEVER_TICKED_GAUGE).set(never as f64);
        self.report_dead_classes(&classes);
        (silent, never, named)
    }

    /// Reports any instrument class that produced NOTHING, once per episode.
    ///
    /// Edge-latched per segment: the rising edge emits, the falling edge
    /// clears the latch so a later recurrence emits again. The gauge is set
    /// unconditionally every sweep, so a dashboard shows the live state while
    /// the log carries one line per episode.
    ///
    /// Log-sink-only by construction. This adds NO Telegram page: the Dhan
    /// alert family is fixed at four items by
    /// `dhan-rest-only-noise-lock-2026-07-14.md` §2, and adding a fifth needs
    /// a dated operator quote in that file FIRST. The counter and gauge are
    /// what an alarm would later read.
    fn report_dead_classes(&self, classes: &ClassLiveness) {
        let previous = self.dead_class_latch.load(Ordering::Relaxed);
        let mut current = 0u8;
        let mut dead_now = 0u64;

        for index in 0..SEGMENT_CLASS_COUNT {
            if !classes.is_dead(index) {
                continue;
            }
            let Some(segment) = segment_class_at(index) else {
                continue;
            };
            dead_now = dead_now.saturating_add(1);
            let bit = 1u8 << index;
            current |= bit;

            if previous & bit != 0 {
                // Already reported this episode — count nothing, log nothing.
                continue;
            }
            metrics::counter!(DEAD_CLASS_METRIC, "segment" => segment.as_str()).increment(1);
            error!(
                code = ErrorCode::RiskGapTickGap.code_str(),
                segment = segment.as_str(),
                instruments = classes.eligible[index],
                "instrument class produced NOTHING since subscribe — every \
                 non-sparse instrument in this segment is still never-ticked \
                 past its warmup window, which is what a subscribe that did \
                 not take looks like; there is no payload to parse and no \
                 error to log, so absence against a seeded key is the only \
                 evidence"
            );
        }

        metrics::gauge!(DEAD_CLASSES_GAUGE).set(dead_now as f64);
        self.dead_class_latch.store(current, Ordering::Relaxed);
    }

    /// Seals every bucket the watermark has moved past, mid-session.
    ///
    /// # Why this has to exist
    ///
    /// A bucket closes when a LATER tick for the SAME instrument crosses its
    /// boundary. For a liquid index that is a non-issue. For an instrument
    /// that ticks once at 10:00:30 and then goes quiet, its 10:00 bar sits
    /// open in memory until the session-close sweep — so a bar that was
    /// complete at 10:01 does not reach the database until 15:30.
    ///
    /// That is a LATENCY defect, not a loss defect, and it is worth being
    /// precise about the difference: the bar is stamped correctly and
    /// `seal_open_buckets_at_close` does eventually write it. Nothing is lost
    /// today. What is lost is the *point* of intraday timeframes — a 1-minute
    /// candle that materialises five hours late is not a 1-minute candle any
    /// consumer can act on.
    ///
    /// `catch_up_seal_all` was written for exactly this and had **zero
    /// production callers** — the third member of the same family as
    /// `force_seal_all` and `scan_silence`.
    ///
    /// # The cutoff, and why it is not just the watermark
    ///
    /// The cutoff is `watermark − CATCHUP_LATENESS_MARGIN_SECS`, never the
    /// watermark itself. The watermark is the highest FOLD-CLOCK second seen
    /// across ALL instruments (receipt where trusted, exchange stamp where not
    /// — corrected 2026-08-28; it said "exchange timestamp", which stopped
    /// being true when the grid moved to the receipt clock), and ticks arrive out of order between them, so
    /// sealing right at the watermark would close a bucket whose own final
    /// ticks are still in flight — turning a latency fix into a truncated-bar
    /// bug, which is strictly worse than the problem it solves. The margin
    /// buys back that reordering window.
    ///
    /// A watermark below the margin (session not started) yields a saturating
    /// zero cutoff, which seals nothing — the correct answer, not a special
    /// case.
    ///
    /// Returns `(emitted, dropped)`, both folded into the same running totals
    /// as the per-tick and close paths: a catch-up drop is as much a loss as
    /// any other and must not be accounted separately.
    ///
    /// # Complexity
    /// O(slots × TF). COLD — every [`CATCHUP_SEAL_INTERVAL`], never per tick.
    pub fn catch_up_seal(&mut self) -> (u64, u64) {
        let cutoff = self
            .aggregator
            .watermark_secs()
            .saturating_sub(CATCHUP_LATENESS_MARGIN_SECS);
        if cutoff == 0 {
            return (0, 0);
        }
        let mut emitted = 0u64;
        let mut dropped = 0u64;
        let mut rescued = 0u64;
        let mut skipped = 0u64;
        let sender = tickvault_storage::seal_writer_runner::global_seal_sender();
        let bars = self.aggregator.catch_up_seal_all(
            cutoff,
            |feed, security_id, segment_code, tf, state| {
                // Emit rows ONLY for the thirteen timeframes the operator
                // asked for (Quote 13, 2026-08-08). The enum carries 24, so
                // eleven second-scale frames — S2 S3 S4 S6 S7 S8 S9 S11 S12
                // S13 S14 — were writing a row per bucket for nobody.
                //
                // Counted into its OWN bucket, never into `dropped`: that
                // counter means data we wanted and lost, and conflating
                // "never asked for it" with "lost it" would make every drop
                // alarm permanently noisy while hiding real losses in the
                // noise. It is counted rather than silently returned because
                // `test_seal_open_buckets_at_close_accounts_every_bar_it_produces`
                // pins that no bar escapes accounting on ANY side — a bare
                // `return` here made bars vanish from the ledger entirely,
                // and that test caught it.
                //
                // The fold still computes all 24 slots. Only emission is
                // gated, so ordinals, the `[_; TF_COUNT]` arrays and the
                // audit-table `timeframe` symbols are all untouched.
                // Pinned by `tf_index::tests::tf_index_operator_set_is_twelve`.
                if !tf.is_operator_requested() {
                    skipped = skipped.saturating_add(1);
                    return;
                }
                let seal = BufferedSeal::new(security_id, segment_code, tf, state, feed);
                // No writer channel installed at all. Before 2026-08-19 this
                // discarded the seal outright; it now takes the same durable
                // route a full channel does, so a boot-order problem costs a
                // disk write rather than a day of candles.
                let Some(tx) = sender else {
                    match escalate_refused_seal(&seal) {
                        SealRefusal::Rescued => rescued = rescued.saturating_add(1),
                        SealRefusal::Lost => dropped = dropped.saturating_add(1),
                    }
                    return;
                };
                // NEVER discard. Operator directive 2026-08-19: "never ever
                // drop any ticks irrespective of any worst case". A refused
                // seal goes to disk (spill, then DLQ); only a seal both disk
                // tiers reject is counted as lost, and that fires
                // AGGREGATOR-DROP-01.
                if let Err(refused) = tx.try_send(seal) {
                    match escalate_refused_seal(&refused.into_inner()) {
                        SealRefusal::Rescued => rescued = rescued.saturating_add(1),
                        SealRefusal::Lost => dropped = dropped.saturating_add(1),
                    }
                } else {
                    emitted = emitted.saturating_add(1);
                }
            },
        );
        debug_assert_eq!(
            bars as u64,
            emitted.saturating_add(dropped),
            "every bar catch_up_seal_all produced must be accounted as emitted or dropped"
        );
        self.seals_emitted = self.seals_emitted.saturating_add(emitted);
        self.seals_dropped = self.seals_dropped.saturating_add(dropped);
        self.seals_rescued = self.seals_rescued.saturating_add(rescued);
        self.seals_skipped = self.seals_skipped.saturating_add(skipped);
        if emitted > 0 {
            counters().seals_emitted.increment(emitted);
        }
        if dropped > 0 {
            counters().seals_dropped.increment(dropped);
        }
        if rescued > 0 {
            counters().seals_rescued.increment(rescued);
        }
        (emitted, dropped)
    }

    /// Instruments the gap detector is tracking. O(1).
    ///
    /// Reported alongside the silent count so the operator can tell "3 of 4
    /// quiet" from "3 of 25,000 quiet" — the same number means very different
    /// things at those two scales.
    #[must_use]
    pub fn tracked_instruments(&self) -> usize {
        self.detector.tracked_instruments()
    }
    /// Observations the silence detector REFUSED because its slot table was
    /// full — the detector's own blindness.
    ///
    /// Non-zero means [`Self::scan_silence`] describes a SUBSET of the
    /// universe while reading exactly as though it describes all of it. The
    /// detector's own doc has always said callers must surface this rather
    /// than assume silence means health; this is the accessor that lets them.
    ///
    /// O(1) — one field read.
    #[must_use]
    pub fn detector_refused(&self) -> u64 {
        self.detector.refused_count()
    }

    /// Sealed candles handed to the process-wide seal writer.
    #[must_use]
    pub const fn seals_emitted(&self) -> u64 {
        self.seals_emitted
    }

    /// Sealed candles LOST — refused by the writer channel AND by both disk
    /// tiers. Since 2026-08-19 this requires the data volume to be unwritable;
    /// a merely-slow writer rescues to disk instead (see
    /// [`Self::seals_rescued`]). Non-zero means candles were computed and are
    /// gone.
    #[must_use]
    pub const fn seals_dropped(&self) -> u64 {
        self.seals_dropped
    }

    /// Sealed candles the writer channel refused that were written to disk
    /// (spill or DLQ) rather than discarded. These are NOT lost — the boot
    /// drain reads them back.
    ///
    /// A sustained non-zero value is a capacity signal: the seal writer is
    /// chronically behind and the lane is paying a disk write per seal to stay
    /// lossless.
    #[must_use]
    pub const fn seals_rescued(&self) -> u64 {
        self.seals_rescued
    }

    /// Bars produced for a timeframe nobody asked for, and therefore not sent.
    ///
    /// Expected to be LARGE and to grow steadily — eleven of the twenty-four
    /// timeframes are unrequested, so on a busy fold this outruns
    /// `seals_emitted`. A big number here is the gate working, not a fault,
    /// which is exactly why it must never be added to `seals_dropped`.
    #[must_use]
    pub const fn seals_skipped(&self) -> u64 {
        self.seals_skipped
    }

    /// Ticks refused because their sequence would not narrow.
    #[must_use]
    pub const fn seq_refused(&self) -> u64 {
        self.seq_refused
    }
}

// ---------------------------------------------------------------------------
// Cached metric handles
// ---------------------------------------------------------------------------

/// Pre-resolved counter handles for the per-frame path.
///
/// The `metrics::counter!` MACRO is not free. With a label it builds a
/// `Key::from_parts(name, vec![Label…])` — **a heap allocation on every call**
/// — and even unlabelled it performs a sharded-registry lookup. Calling it per
/// frame at the ~5,000 frames/sec envelope would allocate millions of times an
/// hour on the exact path this module's docs promise is allocation-free.
///
/// `crates/core/src/parser/dispatcher.rs` hit this first and solved it the same
/// way: resolve every handle ONCE, then `.increment()` the stored handle, which
/// is a plain atomic add. Every label value here is a compile-time-known
/// `&'static str`, so the full set is enumerable up front — there is no
/// unbounded label cardinality hiding in this struct.
pub struct DrainCounters {
    folded: metrics::Counter,
    non_tick: metrics::Counter,
    main_feed_disconnects: metrics::Counter,
    unparseable: metrics::Counter,
    seq_unrepresentable: metrics::Counter,
    aggregator_refused: metrics::Counter,
    write_failed: metrics::Counter,
    ingest_ticks: metrics::Counter,
    ingest_seq_refused: metrics::Counter,
    refused_price: metrics::Counter,
    refused_timestamp: metrics::Counter,
    refused_slot: metrics::Counter,
    refused_session: metrics::Counter,
    seals_emitted: metrics::Counter,
    seals_dropped: metrics::Counter,
    seals_rescued: metrics::Counter,
    flush_ok: metrics::Counter,
    flush_failed: metrics::Counter,
    /// Inline-depth (`d5`) flush failures. Separate from `flush_failed` so a
    /// depth-writer problem is never mistaken for tick loss, which is a far
    /// more serious signal.
    depth_flush_failed: metrics::Counter,
    depth_unconsumed: metrics::Counter,
    /// Depth frames and inline-depth packets DROPPED because the ingest-shed
    /// gate is closed (`tickvault_common::ingest_shed`). Separate from every
    /// other depth counter on purpose: these rows were not refused as bad, and
    /// nothing failed — the box deliberately stopped writing them to stay
    /// alive on a full disk, and reading that as corruption would send an
    /// operator hunting a bug that does not exist.
    shed_inline_depth: metrics::Counter,
    shed_dedicated_depth: metrics::Counter,
    depth_rows: metrics::Counter,
    depth_refused: metrics::Counter,
    depth_dropped: metrics::Counter,
    depth_disconnects: metrics::Counter,
    depth_length_mismatch: metrics::Counter,
    truncated: metrics::Counter,
    /// Bytes abandoned mid-frame by the two give-up arms. See
    /// [`DRAIN_ABANDONED_BYTES_COUNTER`] for why this is bytes and not packets.
    abandoned_bytes: metrics::Counter,
    xverify_measured: metrics::Counter,
    xverify_vacuous: metrics::Counter,
    xverify_failed: metrics::Counter,
    xverify_no_token: metrics::Counter,
}

impl DrainCounters {
    /// The refusal counter for one reason. Total over the reason set the fold
    /// can produce; an unknown reason falls back to the session bucket rather
    /// than allocating a fresh key on the hot path.
    fn refused(&self, reason: &str) -> &metrics::Counter {
        match reason {
            "price" => &self.refused_price,
            "timestamp" => &self.refused_timestamp,
            "slot_exhausted" => &self.refused_slot,
            _ => &self.refused_session,
        }
    }
}

/// Process-wide handle set, resolved on first use.
/// The process-wide cached handle set.
///
/// `pub` so the DHAT gate can drive `drain_main_feed_frame`. Handing the test
/// the SAME `OnceLock` the production path uses is the point: a gate that
/// built its own counters would measure a different function than the one that
/// ships, and the whole reason these handles exist is that resolving a metric
/// key per frame allocates.
// A OnceLock accessor over metrics handles: no branch to assert. Its REASON to
// be pub is exercised by dhat_live_ingest_seam.rs, which drives
// drain_main_feed_frame with these exact handles.
// TEST-EXEMPT: OnceLock accessor with no branch; its purpose is covered by dhat_live_ingest_seam.rs
pub fn counters() -> &'static DrainCounters {
    static COUNTERS: std::sync::OnceLock<DrainCounters> = std::sync::OnceLock::new();
    COUNTERS.get_or_init(|| DrainCounters {
        folded: metrics::counter!(DRAIN_FRAMES_COUNTER, "outcome" => "folded"),
        non_tick: metrics::counter!(DRAIN_FRAMES_COUNTER, "outcome" => "non_tick"),
        main_feed_disconnects: metrics::counter!(DRAIN_FRAMES_COUNTER, "outcome" => "disconnect"),
        unparseable: metrics::counter!(DRAIN_FRAMES_COUNTER, "outcome" => "unparseable"),
        seq_unrepresentable: metrics::counter!(DRAIN_FRAMES_COUNTER, "outcome" => "seq_unrepresentable"),
        aggregator_refused: metrics::counter!(DRAIN_FRAMES_COUNTER, "outcome" => "aggregator_refused"),
        write_failed: metrics::counter!(DRAIN_FRAMES_COUNTER, "outcome" => "write_failed"),
        ingest_ticks: metrics::counter!(INGEST_TICKS_COUNTER),
        ingest_seq_refused: metrics::counter!(INGEST_SEQ_REFUSED_COUNTER),
        refused_price: metrics::counter!(INGEST_REFUSED_COUNTER, "reason" => "price"),
        refused_timestamp: metrics::counter!(INGEST_REFUSED_COUNTER, "reason" => "timestamp"),
        refused_slot: metrics::counter!(INGEST_REFUSED_COUNTER, "reason" => "slot_exhausted"),
        refused_session: metrics::counter!(INGEST_REFUSED_COUNTER, "reason" => "out_of_session"),
        seals_emitted: metrics::counter!(SEALS_EMITTED_COUNTER),
        seals_dropped: metrics::counter!(SEALS_DROPPED_COUNTER),
        seals_rescued: metrics::counter!(SEALS_RESCUED_COUNTER),
        flush_ok: metrics::counter!(FLUSH_COUNTER, "outcome" => "ok"),
        flush_failed: metrics::counter!(FLUSH_COUNTER, "outcome" => "failed"),
        depth_flush_failed: metrics::counter!(FLUSH_COUNTER, "outcome" => "depth_failed"),
        depth_unconsumed: metrics::counter!(DRAIN_FRAMES_COUNTER, "outcome" => "depth_unconsumed"),
        shed_inline_depth: metrics::counter!(DEPTH_COUNTER, "outcome" => "shed_inline"),
        shed_dedicated_depth: metrics::counter!(DEPTH_COUNTER, "outcome" => "shed_dedicated"),
        depth_rows: metrics::counter!(DEPTH_COUNTER, "outcome" => "rows"),
        depth_refused: metrics::counter!(DEPTH_COUNTER, "outcome" => "refused"),
        depth_dropped: metrics::counter!(DEPTH_COUNTER, "outcome" => "dropped"),
        depth_disconnects: metrics::counter!(DEPTH_COUNTER, "outcome" => "disconnects"),
        depth_length_mismatch: metrics::counter!(DEPTH_COUNTER, "outcome" => "length_mismatch"),
        truncated: metrics::counter!(DRAIN_FRAMES_COUNTER, "outcome" => "truncated"),
        abandoned_bytes: metrics::counter!(DRAIN_ABANDONED_BYTES_COUNTER),
        xverify_measured: metrics::counter!(XVERIFY_RUNS_COUNTER, "outcome" => "measured"),
        xverify_vacuous: metrics::counter!(XVERIFY_RUNS_COUNTER, "outcome" => "vacuous"),
        xverify_failed: metrics::counter!(XVERIFY_RUNS_COUNTER, "outcome" => "failed"),
        xverify_no_token: metrics::counter!(XVERIFY_RUNS_COUNTER, "outcome" => "no_token"),
    })
}

/// Counter: daily cross-verification attempts, by outcome. Anything other than
/// `measured` means the session's captured candles were never checked against
/// Dhan's own record.
///
/// # Why `vacuous` is its own label and not folded into `ran`
///
/// It WAS folded in, and the doc above said "anything other than `ran`" while
/// a run that compared ZERO minutes counted as `ran`. So the one label an
/// operator would read as "we checked" was also what a run that proved nothing
/// reported -- the comment promised a guarantee the label could not deliver.
///
/// That matters more here than almost anywhere else: this comparison is the
/// revived feed's ONLY ground truth, the `compared == 0` case is the exact
/// false-OK this repository has retired twice, and the comparator itself is
/// scrupulous about it -- `Blind` is a first-class outcome, `is_pass()` is
/// false for it, and a vacuous run fires a coded `error!`. The gap was never
/// the detection; it was that every DELIVERY surface flattened the distinction.
pub const XVERIFY_RUNS_COUNTER: &str = "tv_dhan_feed_xverify_runs_total";

/// Counter: sealed candles handed to the process-wide seal writer.
pub const SEALS_EMITTED_COUNTER: &str = "tv_dhan_feed_seals_emitted_total";

/// Counter: sealed candles LOST — computed, refused by the writer channel,
/// AND refused by both disk tiers. Since 2026-08-19 this requires the data
/// volume to be unwritable; a merely-slow writer no longer reaches it.
///
/// Non-zero means candles were computed and are gone. Paired with
/// AGGREGATOR-DROP-01 (Critical, paged).
pub const SEALS_DROPPED_COUNTER: &str = "tv_dhan_feed_seals_dropped_total";

/// Counter: sealed candles the writer channel refused that were RESCUED to
/// disk (spill or DLQ) instead of discarded.
///
/// Added 2026-08-19 with the no-drop policy. It is deliberately its own name
/// rather than an outcome label on `seals_dropped`: these candles are NOT
/// lost — they are on disk awaiting the boot drain — and folding them into a
/// loss counter would make the loss alarm fire for a working rescue. A
/// sustained non-zero value means the seal writer is chronically behind,
/// which is a capacity signal, not a data-loss one.
pub const SEALS_RESCUED_COUNTER: &str = "tv_dhan_feed_seals_rescued_total";

/// Counter: everything that happens to a depth packet, by `outcome`.
///
/// ONE name with an `outcome` label rather than five names, matching
/// [`DRAIN_FRAMES_COUNTER`]'s shape — and here the choice is forced as well as
/// consistent. The CloudWatch EMF selector matches on NAME, and it lives inside
/// `user-data.sh.tftpl`, which is 512 bytes from EC2's hard 16 KiB user-data
/// limit. Five names did not fit; one does, and it ships every outcome rather
/// than making someone choose which losses are worth seeing.
///
/// Outcomes:
/// * `rows` — level rows appended. Counts ROWS, not frames: one depth-200
///   side-packet is 200 rows and one depth-20 side-packet is 20, so a frame
///   count would make the two pools look comparable when their volumes differ
///   by an order of magnitude.
/// * `refused` — parse error, unmappable segment code, truncated frame tail,
///   or an ILP append failure. The honest counterpart to "nothing is dropped":
///   non-zero means levels that ARRIVED are NOT in the table.
/// * `dropped` — parsed and buffered, then lost at a failed flush. A DIFFERENT
///   failure from `refused` and deliberately a separate outcome: one is the
///   feed, the other is the database.
/// * `disconnects` — server-initiated disconnect packets on a depth socket.
/// * `length_mismatch` — vendor `message_length` disagreed with the length
///   derived from the protocol shape. The derived length is authoritative so
///   no data is lost, but a sustained non-zero reading means the vendor
///   changed a convention, and learning that from a counter beats learning it
///   from mis-framed books. Expected 0; UNVERIFIED-LIVE.
pub const DEPTH_COUNTER: &str = "tv_dhan_feed_depth_total";

/// Counter: ILP flushes to QuestDB, by outcome.
pub const FLUSH_COUNTER: &str = "tv_dhan_feed_flush_total";

// ---------------------------------------------------------------------------
// Frame drain — the socket→fold edge
// ---------------------------------------------------------------------------

/// Frames the bounded ring holds between the read tasks and the fold.
///
/// Sized for a burst, not a backlog: at the ~5,000 frames/sec envelope this is
/// roughly thirteen seconds of head-room, which covers a GC-style stall in the
/// fold without letting an unbounded queue eat the heap. A full ring is a lag
/// signal, never capture loss — the frame is already durable in the WAL by the
/// time `try_send` is attempted (`WalRingSink`).
pub const FRAME_RING_CAPACITY: usize = 65_536;

/// How many times the lane re-checks for the token manager before refusing.
///
/// Sized against what it is actually waiting for: `TokenManager::initialize`
/// performs SSM credential reads, a TOTP computation and an HTTPS
/// `generateAccessToken` round-trip inside a retry loop whose backoff floor is
/// >=130s. One retry cycle therefore has to fit, or a single transient auth
/// failure would still cost the whole session. 60 attempts x 5s = 5 minutes,
/// which covers a first attempt plus a full backoff plus a second attempt,
/// and still lands well before the 09:15 IST open on the 08:30 boot schedule.
pub const TOKEN_MANAGER_WAIT_ATTEMPTS: u64 = 60;

/// Seconds between token-manager re-checks. Short enough that the common case
/// — the manager appearing within a second or two of a fast auth — costs
/// almost nothing, since the loop exits on the first success.
pub const TOKEN_MANAGER_WAIT_INTERVAL_SECS: u64 = 5;

/// The ring's byte ceiling — the bound the frame count alone does not give.
///
/// `FRAME_RING_CAPACITY` bounds how MANY frames sit in the ring, not how much
/// memory they occupy, and `CapturedFrame` owns a `Bytes` of peer-chosen
/// length up to `max_frame_bytes(endpoint)`: 256 KiB on the main feed, 512 KiB
/// on depth-200. 65,536 × 256 KiB is **16 GiB**, and 65,536 × 512 KiB is
/// **32 GiB** — the whole r8g.xlarge, held by a queue whose own doc comment
/// called it a bounded burst absorber. The count bound is real; it just does
/// not bound the thing that runs out.
///
/// 256 MiB, chosen so it is INERT in normal operation and decisive outside it.
/// A real Dhan Quote packet is 50 bytes and a frame batches a handful, so at
/// realistic sizes the count bound is reached at roughly 64 MiB resident and
/// this ceiling is never consulted — behaviour is unchanged on every normal
/// day. It engages only when frames are large AND the fold has stalled, which
/// is simultaneously the hostile-peer shape and the genuine-stall shape, and
/// caps both at a quarter gigabyte instead of the host.
///
/// It is deliberately NOT sized as "N seconds of traffic": that framing is what
/// produced a count-only bound in the first place. This is a memory ceiling,
/// and the unit it is expressed in is the unit that runs out.
pub const FRAME_RING_MAX_BYTES: usize = 256 * 1024 * 1024;

/// Hard ceiling for the auto-sized ring. Above this the ring stops being a
/// burst absorber and starts competing with QuestDB for the same RAM.
pub const FRAME_RING_MAX_BYTES_CEILING: usize = 2 * 1024 * 1024 * 1024;

/// Share of host RAM the ring may occupy when auto-sizing.
///
/// 2% of a 32 GiB host is 655 MiB — roughly 2.5× today's fixed value, still
/// under a fortieth of the machine, and comfortably clear of the ~14–31 GiB
/// the sizing note budgets for QuestDB, the tick set and the OS.
pub const FRAME_RING_RAM_PERCENT: usize = 2;

/// Total host RAM in bytes, read from `/proc/meminfo`.
///
/// Deliberately parses `/proc/meminfo` rather than taking a dependency: adding
/// a crate needs operator approval, and this is one integer from a file that
/// has had the same format for decades. `None` on anything unexpected — a host
/// whose memory cannot be read must fall back, never guess.
fn host_total_ram_bytes() -> Option<usize> {
    parse_meminfo_total_bytes(&std::fs::read_to_string("/proc/meminfo").ok()?)
}

/// Pure `/proc/meminfo` → total-RAM-in-bytes parser.
///
/// Split out from the file read so hostile input is REACHABLE by a test. The
/// previous shape read the live file inline, which meant the only input this
/// code could ever be tested against was the one the CI runner happened to
/// have — every malformed, truncated, unit-shifted or adversarial variant was
/// unreachable, and "it works on this machine" was the whole of the evidence.
///
/// # What it refuses, and why each refusal is load-bearing
///
/// - **No `MemTotal:` line** → `None`. A file that does not answer the question
///   must not be guessed at.
/// - **Unit token that is not `kB`** → `None`. This is the one that silently
///   costs a factor of 1024: every Linux kernel to date reports kB, but a value
///   read as kB when it is bytes under-sizes the ring by 1024× (invisible — it
///   just clamps to the floor and looks normal), and read as bytes when it is
///   kB over-sizes by 1024× (clamps to the ceiling, equally quiet). Requiring
///   the unit turns a silent mis-scale into an explicit fallback.
/// - **Non-numeric, negative, or empty value** → `None`.
/// - **Multiplication overflow** → `None` (checked, never wrapping).
///
/// A `MemTotal:` line appearing more than once takes the FIRST — matching the
/// kernel's own single-line contract rather than inventing a merge rule.
fn parse_meminfo_total_bytes(meminfo: &str) -> Option<usize> {
    let rest = meminfo
        .lines()
        .find_map(|line| line.strip_prefix("MemTotal:"))?;

    let mut fields = rest.split_whitespace();
    let kb: usize = fields.next()?.parse().ok()?;

    // The unit is mandatory. See the doc comment: a missing unit is exactly
    // where a 1024× mis-scale hides, and both directions of that error are
    // silent because the clamps absorb them.
    match fields.next() {
        Some("kB") => kb.checked_mul(1024),
        _ => None,
    }
}

/// Pure sizing arithmetic: host RAM (or `None`) → ring byte budget.
///
/// Separated from [`frame_ring_max_bytes_for_host`] so the bounds can be proven
/// against the REAL function across the whole input range, including values no
/// machine this runs on will ever report. A test that re-implements the formula
/// in its own closure proves the copy, not the code.
fn ring_bytes_for_ram(total_ram_bytes: Option<usize>) -> usize {
    match total_ram_bytes {
        Some(total) => (total / 100)
            .saturating_mul(FRAME_RING_RAM_PERCENT)
            .clamp(FRAME_RING_MAX_BYTES, FRAME_RING_MAX_BYTES_CEILING),
        None => FRAME_RING_MAX_BYTES,
    }
}

/// The ring byte ceiling for THIS host.
///
/// # Why this is not a constant
///
/// Until 2026-08-15 every buffer in this lane was a fixed number. The host was
/// upgraded 4 GiB → 8 → 16 → 32 GiB across four operator decisions and **not
/// one of these values moved**, so a 32 GiB machine ran the ring sized for a
/// 4 GiB one — 0.8% of the box. The instance grew; the software never noticed.
///
/// That is the "dynamic, scalable" property the charter asks for, absent in
/// the one place it costs money.
///
/// # The bounds are the safety, not the percentage
///
/// - **Floor = [`FRAME_RING_MAX_BYTES`]**, today's proven value. Auto-sizing can
///   only ever grow the budget, so a small or unreadable host lands exactly
///   where it is now. This can never regress a working configuration.
/// - **Ceiling = [`FRAME_RING_MAX_BYTES_CEILING`]**, so a very large host does
///   not hand the ring memory QuestDB needs.
/// - **Unreadable `/proc/meminfo` → the floor.** Fail to the known-good value,
///   never to a guess.
pub fn frame_ring_max_bytes_for_host() -> usize {
    ring_bytes_for_ram(host_total_ram_bytes())
}

// A ceiling below the largest single admissible frame would refuse EVERY frame
// from that endpoint — a total feed outage wearing the shape of backpressure.
// This asserts a real margin above that floor, so the byte bound stays a burst
// absorber rather than becoming a gate.
//
// The margin is 64×, not the 512× today's numbers happen to give
// (256 MiB / 512 KiB = exactly 512 depth-200 frames, or 1,024 main-feed ones).
// A first draft of this line asserted the coincidental figure and failed the
// build on its own arithmetic — a `>` against an exactly-equal value. A
// const-assert should pin the PROPERTY that must hold, not today's quotient;
// pinning the quotient turns any future re-sizing into a spurious failure and
// teaches the next reader to edit the assertion instead of thinking about it.
const _: () = assert!(
    FRAME_RING_MAX_BYTES > tickvault_core::websocket::connection::DEPTH_200_MAX_FRAME_BYTES * 64,
    "FRAME_RING_MAX_BYTES must hold many maximum-size frames, or the byte budget \
     stops being a burst absorber and becomes a refusal gate"
);

/// The main feed's share of the ring's byte ceiling.
///
/// 2026-08-14: the ceiling above used to be ONE budget shared by all sixteen
/// sockets, and the comment at its call site argued for that — "the heap it
/// protects is one heap". True, and it still refused to bound five times the
/// host's memory. But sharing one budget across endpoints that carry different
/// PAYLOADS has a failure mode the memory argument misses:
///
/// a depth-200 frame may be 512 KiB against a main-feed frame's ~4 KiB, so
/// roughly **512 depth frames exhaust the entire budget** — and every main-feed
/// frame behind them is then refused. So one endpoint's burst could starve the
/// other, and the endpoint with the 128x larger frame always wins that race.
///
/// CORRECTED 2026-08-20: this paragraph used to end "Depth frames are
/// `depth_unconsumed`: counted and DISCARDED, because nothing folds them
/// today" — i.e. the argument that the evicting stream was one we threw away.
/// That stopped being true on 2026-08-15, when depth gained a writer and a
/// table. The SPLIT is still right and the number is unchanged; the reason is
/// now simply frame-size asymmetry, which needs no claim about which stream
/// matters. Recorded rather than silently reworded, because a reader
/// re-deriving this budget from the old sentence would conclude depth stores
/// nothing and could shrink its share — which would now drop real rows.
///
/// Splitting the same total keeps the memory ceiling identical (the host is no
/// worse off) and makes that eviction impossible: depth can exhaust depth.
pub const MAIN_FEED_RING_MAX_BYTES: usize = FRAME_RING_MAX_BYTES * 3 / 4;

/// Depth's share of the same ceiling — the other quarter, for both depth-20 and
/// depth-200 together.
///
/// The split is 3:1 toward the main feed rather than by socket count (5 vs 10)
/// because it follows the FRAME SIZE, not the socket count: a depth-200 frame
/// is ~128x a main-feed frame, so equal byte budgets would be wildly unequal
/// frame budgets.
///
/// CORRECTED 2026-08-20: this used to read "the split follows the DATA … the
/// main feed carries every tick that reaches the database, and depth currently
/// carries none." Depth has carried rows to `market_depth` since 2026-08-15.
/// The 3:1 number is UNCHANGED and still defensible on frame size; only its
/// stated reason was stale, and a stale reason on a live memory constant is
/// what a future reader re-derives from.
///
/// A quarter of 256 MiB is 64 MiB — still 128 maximum-size depth-200 frames,
/// so depth keeps a real burst absorber rather than a token allocation.
pub const DEPTH_RING_MAX_BYTES: usize = FRAME_RING_MAX_BYTES - MAIN_FEED_RING_MAX_BYTES;

// The split must remain exhaustive: any drift turns a memory ceiling into
// either an over-commitment of the host or a silently smaller ring.
const _: () = assert!(
    MAIN_FEED_RING_MAX_BYTES + DEPTH_RING_MAX_BYTES == FRAME_RING_MAX_BYTES,
    "the per-endpoint budgets must sum to the total, or the host ceiling moved"
);
// Each share must still clear the same floor the total does, for the same
// reason: a share below the largest admissible frame from its own endpoint
// refuses every frame and reads as backpressure rather than as an outage.
const _: () = assert!(
    DEPTH_RING_MAX_BYTES > tickvault_core::websocket::connection::DEPTH_200_MAX_FRAME_BYTES * 64,
    "the depth share must hold many maximum-size depth frames"
);
const _: () = assert!(
    MAIN_FEED_RING_MAX_BYTES
        > tickvault_core::websocket::connection::DEPTH_200_MAX_FRAME_BYTES * 64,
    "the main-feed share must hold many maximum-size frames"
);

/// Counter: frames taken off the ring, labelled by what the parser made of
/// them.
pub const DRAIN_FRAMES_COUNTER: &str = "tv_dhan_feed_drain_frames_total";

/// Counter: bytes of a frame ABANDONED without being decoded.
///
/// A single WebSocket message stacks up to ~1,600 packets. When the walk hits
/// an unrecognised response code or a trailing partial packet it stops — and
/// stopping is the RIGHT call, because resynchronising on a guess would
/// fabricate ticks out of misaligned bytes, which is worse than losing them.
///
/// What was wrong was the ACCOUNTING. Both arms incremented the frame counter
/// by ONE, so a frame that dropped 1,500 packets and a frame that dropped one
/// reported the same number, and an operator reading `unparseable = 1` would
/// reasonably conclude a single bad packet. This records the magnitude the
/// outcome counter cannot: the remaining byte count at the moment the walk
/// gave up. Non-zero here means ticks were lost and says roughly how many.
///
/// Deliberately BYTES, not packets: the packet count of the remainder is
/// unknowable — that is exactly what "we could not decode it" means — and a
/// divide-by-typical-size estimate would be a fabricated number in a counter
/// whose whole purpose is to stop fabrication.
///
/// Not EMF-selected today, so it is visible on `/metrics` and not in
/// CloudWatch. Stated rather than assumed: shipping it is a cost decision
/// (~$0.30/mo) that belongs with the alarm that would read it.
pub const DRAIN_ABANDONED_BYTES_COUNTER: &str = "tv_dhan_feed_abandoned_bytes_total";

/// Gauge: the LONGEST a frame sat in the ring before the drain folded it, in
/// milliseconds, over the last reporting window.
///
/// # Why this one and not the vendor lag
///
/// This number was already being computed on every frame — ~5,000 times a
/// second — and immediately thrown away. `run_frame_drain` derives
/// `queued_nanos` from the frame's own monotonic receipt stamp solely to
/// back-date `received_at_nanos`, then drops it. Publishing costs one
/// comparison per frame and one gauge write per window.
///
/// It is the signal that actually predicts the failure the lane is judged on.
/// `tv_dhan_ws_lag_ms` measures how long DHAN took to deliver — their problem,
/// unfixable from here, and deliberately excluded from the EMF selector as a
/// "how much" rather than a "what broke" (see `EMF-METRIC-SELECTOR-NOTES.md`).
/// Ring dwell measures how far behind OUR OWN drain is, which is the direct
/// precursor of every loss mechanism the lane has: a drain that stops draining
/// fills the ring, and a full ring refuses frames.
///
/// # MAX, not mean, and not a histogram
///
/// A mean hides the stall that matters — one 8-second dwell inside a window of
/// microsecond dwells averages to nothing. A histogram would ship ~12 bucket
/// series per dimension and cost roughly an order of magnitude more than the
/// per-connection latency figure the 2026-08-14 authorization priced at
/// $4.80/mo; that discrepancy is recorded in the noise lock rather than spent
/// silently. One gauge is one series.
///
/// # Cost on the hot path
///
/// One `i64` max against a stack local per frame. No allocation, no registry
/// lookup, no label — the gauge handle is resolved once and written on the
/// existing `DRAIN_REPORT_EVERY` path beside `publish_fold_depth`, never per
/// frame. This is the `DrainCounters` discipline the module already documents.
///
/// Reset to zero after each publish, deliberately: a sticky maximum reads
/// alarming forever after one stall, which is how a signal stops being read.
pub const RING_DWELL_MAX_MS_GAUGE: &str = "tv_dhan_feed_ring_dwell_max_ms";

/// Gauge: rows appended to the ILP buffer but not yet flushed to QuestDB.
/// A buffer is a staging area, not storage — a rising value means rows are
/// accumulating in the process rather than landing in the database.
pub const PENDING_ROWS_GAUGE: &str = "tv_dhan_feed_pending_rows";

/// Gauge: sealed candles discarded this session (no seal writer, or its queue
/// was full). Any non-zero value means candles were computed and thrown away.
pub const SEALS_DROPPED_GAUGE: &str = "tv_dhan_feed_seals_dropped";

/// Gauge: ticks refused because their frame sequence would not narrow onto
/// `capture_seq`. A counted loss, never a silent stamp.
pub const SEQ_REFUSED_GAUGE: &str = "tv_dhan_feed_seq_refused";

/// Gauge: subscribed instruments that are quiet beyond their own learned
/// cadence, excluding legitimately-sparse ones. The read-out of the gap
/// detector the lane has always fed and never questioned.
pub const INSTRUMENTS_SILENT_GAUGE: &str = "tv_dhan_feed_instruments_silent";

/// Gauge: subscribed instruments that have produced NOTHING since being
/// seeded. Distinct from the gauge above because the cause is different: a
/// never-ticked instrument usually means the subscribe did not take, and a
/// silently-unsubscribed instrument is invisible in every other signal the
/// lane produces — there is no payload to count, no parse to fail, and no
/// error to log. Absence against a seeded key is the only evidence.
pub const INSTRUMENTS_NEVER_TICKED_GAUGE: &str = "tv_dhan_feed_instruments_never_ticked";

/// Counter: an entire instrument CLASS produced nothing, once per episode.
///
/// # Why a class detector exists beside a per-instrument one
///
/// [`INSTRUMENTS_NEVER_TICKED_GAUGE`] counts instruments. It cannot answer
/// the question that actually matters when a subscribe silently fails for one
/// SEGMENT: on 2026-08-21 the lane subscribed 119 NSE indices and received
/// **zero** ticks from any of them for the whole session, while 8,868
/// tradeable instruments flowed normally at 17.5M ticks. The per-instrument
/// gauge read 119 out of ~9,000 — under 1.5%, indistinguishable at a glance
/// from ordinary thin-instrument quiet, and nothing paged. It was found by a
/// human reading logs.
///
/// A whole class producing nothing is a different fact with a different
/// cause: the subscribe did not take for that segment. `IDX_I` in Full mode
/// is the known instance — an index has no order book, so asking for depth-5
/// requests something that does not exist and Dhan answers with silence
/// rather than an error. That failure is invisible to every other signal the
/// lane produces, because absence has no payload to parse and no error to
/// log.
pub const DEAD_CLASS_METRIC: &str = "tv_dhan_feed_dead_instrument_class_total";

/// Gauge: instrument classes currently judged dead. `0` is the healthy value.
pub const DEAD_CLASSES_GAUGE: &str = "tv_dhan_feed_dead_instrument_classes";

/// Number of [`ExchangeSegment`] variants — the width of the class tallies.
///
/// A fixed array rather than a map: the count is a compile-time property of
/// the enum, so the rollup stays O(1) per report and allocation-free, which
/// matters because it rides inside the O(n) silence sweep rather than adding
/// a second pass over the universe.
const SEGMENT_CLASS_COUNT: usize = 8;

/// Dense index for a segment. Explicit match, never a discriminant cast, so
/// re-ordering the enum cannot silently re-label a class's tallies.
const fn segment_class_index(segment: ExchangeSegment) -> usize {
    match segment {
        ExchangeSegment::IdxI => 0,
        ExchangeSegment::NseEquity => 1,
        ExchangeSegment::NseFno => 2,
        ExchangeSegment::NseCurrency => 3,
        ExchangeSegment::BseEquity => 4,
        ExchangeSegment::McxComm => 5,
        ExchangeSegment::BseCurrency => 6,
        ExchangeSegment::BseFno => 7,
    }
}

/// Inverse of [`segment_class_index`], for labelling an episode.
const fn segment_class_at(index: usize) -> Option<ExchangeSegment> {
    match index {
        0 => Some(ExchangeSegment::IdxI),
        1 => Some(ExchangeSegment::NseEquity),
        2 => Some(ExchangeSegment::NseFno),
        3 => Some(ExchangeSegment::NseCurrency),
        4 => Some(ExchangeSegment::BseEquity),
        5 => Some(ExchangeSegment::McxComm),
        6 => Some(ExchangeSegment::BseCurrency),
        7 => Some(ExchangeSegment::BseFno),
        _ => None,
    }
}

/// Per-segment liveness tally, folded from the silence sweep.
///
/// # The three buckets, and why a naive two-bucket version is wrong
///
/// `eligible` is the denominator: every seeded instrument in the segment that
/// we are willing to judge. `never` is the numerator: those that have
/// produced nothing AND have had a fair chance to. `pending` is the ones
/// still inside their fair-chance window.
///
/// `pending` is what stops a false page on every boot. Between subscribing
/// and the first tick every instrument is legitimately never-ticked, so a
/// detector without this bucket would declare every class dead a few seconds
/// after connect, every single morning.
///
/// **Sparse instruments are excluded from ALL THREE.** Far-month futures and
/// INDIA VIX are legitimately quiet for minutes at a time and the scope lock
/// already excludes them from the silent count; judging them here would
/// manufacture the alarm this detector exists to make trustworthy. Excluding
/// them from the denominator too — not just the numerator — is the part that
/// is easy to get wrong: leaving them in the denominator would make
/// `never == eligible` unreachable for any segment containing one, and the
/// detector would silently never fire. That is the false-OK class this
/// repository has retired twice.
#[derive(Debug, Default, Clone, Copy)]
struct ClassLiveness {
    eligible: [u32; SEGMENT_CLASS_COUNT],
    never: [u32; SEGMENT_CLASS_COUNT],
    pending: [u32; SEGMENT_CLASS_COUNT],
}

impl ClassLiveness {
    /// Folds one silence report. O(1), no allocation.
    fn observe(
        &mut self,
        segment: ExchangeSegment,
        sparse: bool,
        never_ticked: bool,
        past_window: bool,
    ) {
        if sparse {
            return;
        }
        let i = segment_class_index(segment);
        self.eligible[i] = self.eligible[i].saturating_add(1);
        if never_ticked {
            if past_window {
                self.never[i] = self.never[i].saturating_add(1);
            } else {
                self.pending[i] = self.pending[i].saturating_add(1);
            }
        }
    }

    /// True when this segment produced NOTHING and every member has had its
    /// fair chance.
    ///
    /// An instrument that ticked and then went quiet carries the `Exceeded`
    /// verdict, not `NeverTicked`, so it counts in `eligible` and keeps the
    /// class alive: this fires for "never produced anything", never for "has
    /// gone quiet".
    ///
    /// # The `pending` term is REDUNDANT, and that is recorded rather than hidden
    ///
    /// `pending == 0` reads like the warmup guard, and it was written as one.
    /// It is not load-bearing: `observe` increments `eligible` for every
    /// non-sparse instrument but `never` only for past-window ones, so
    /// `pending > 0` already forces `never < eligible` and the equality below
    /// fails on its own. Mutating this term away does not change a single
    /// verdict — proven by bite-testing it, which is how the redundancy was
    /// found at all.
    ///
    /// It is KEPT because it states the intent that the arithmetic only
    /// implies, and it costs one comparison on a path that runs eight times
    /// per 30-second sweep. What makes that safe rather than decorative is
    /// `the_tally_invariant_that_makes_the_pending_term_redundant_holds`,
    /// which pins the relationship the redundancy depends on — so a future
    /// change to the fold that broke it would fail a test instead of silently
    /// turning this into the warmup guard everyone already believes it is.
    fn is_dead(&self, index: usize) -> bool {
        self.eligible[index] > 0
            && self.pending[index] == 0
            && self.never[index] == self.eligible[index]
    }
}

/// How many silent instruments a single episode may NAME in the log.
///
/// The page fires once per episode behind a 30-minute cooldown, so this bounds
/// what one episode can write. Unbounded naming at the 25,000-instrument
/// target would be a 25,000-line burst into the sink, which buries the signal
/// it is reporting — and the operator does not need all of them to act: a
/// subscribe that did not take fails in groups, so the quietest handful
/// identifies the group.
pub const WORST_SILENT_NAMED: usize = 8;

/// One named silent instrument, for the log line.
///
/// `Copy` and scalar-only so ranking stays allocation-free on a path that
/// already sweeps every tracked instrument.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SilentInstrument {
    pub security_id: u64,
    pub segment: ExchangeSegment,
    /// How long it has been quiet.
    pub silent_millis: u64,
    /// The cadence it had EARNED before going quiet — the number that makes
    /// the silence meaningful. A contract that ticks once an hour being quiet
    /// for five minutes is not news; an index quiet for five minutes is.
    pub expected_millis: u64,
    /// True when it has produced nothing at all since being subscribed, which
    /// usually means the subscribe did not take.
    pub never_ticked: bool,
}

impl SilentInstrument {
    /// Filler for the caller's fixed buffer. `security_id` 0 is not a real
    /// instrument, so an unfilled slot is recognisable if one ever escapes
    /// the `named` bound.
    pub const EMPTY: Self = Self {
        security_id: 0,
        segment: ExchangeSegment::IdxI,
        silent_millis: 0,
        expected_millis: 0,
        never_ticked: false,
    };
}

/// Insert `candidate` into a descending-by-silence top-K buffer.
///
/// Returns the new filled length. Pure, `O(K)`, no allocation — extracted so
/// the ordering is testable on its own rather than only reachable through a
/// full detector sweep.
///
/// Ties keep the incumbent: a scan that reports many instruments at exactly
/// the same silence (the shape a failed subscribe produces, since they all
/// went quiet together) then names them in detector order rather than
/// reshuffling on every equal comparison.
#[must_use]
pub fn rank_silent(
    worst: &mut [SilentInstrument; WORST_SILENT_NAMED],
    filled: usize,
    candidate: SilentInstrument,
) -> usize {
    let mut pos = filled.min(WORST_SILENT_NAMED);
    while pos > 0 && worst[pos - 1].silent_millis < candidate.silent_millis {
        pos -= 1;
    }
    if pos >= WORST_SILENT_NAMED {
        return filled.min(WORST_SILENT_NAMED);
    }
    let end = filled.min(WORST_SILENT_NAMED - 1);
    let mut i = end;
    while i > pos {
        worst[i] = worst[i - 1];
        i -= 1;
    }
    worst[pos] = candidate;
    (filled + 1).min(WORST_SILENT_NAMED)
}

/// Gauge: observations the silence detector REFUSED because its slot table
/// was full. Non-zero means the two gauges above describe only part of the
/// universe while reading as though they describe all of it — the detector is
/// blind, and a blind detector reporting zero silent instruments looks exactly
/// like a healthy feed.
pub const SILENCE_DETECTOR_REFUSED_GAUGE: &str = "tv_dhan_feed_silence_detector_refused";

/// Gauge: seconds since the lane last persisted a tick, or — when no tick has
/// arrived at all this session — seconds since the drain started.
///
/// # Why a gauge exists for something a counter already counts
///
/// `tv_dhan_feed_ingest_ticks_total` answers "is the feed producing anything",
/// and the alarm that reads it is the one signal in this lane that separates
/// "nothing broke" from "nothing ran". But it answers it through a COUNTER,
/// and a counter's meaning in CloudWatch depends on a pipeline detail this
/// repository has never been able to verify from the sandbox: whether the
/// agent publishes each scrape's DELTA or the running CUMULATIVE total.
///
/// The two readings are not equally forgiving. Under deltas, `Sum < 1` over a
/// window means no ticks folded in that window — correct. Under cumulative
/// values, `Sum` is roughly five times the session total and `< 1` can never
/// be true once a single tick has ever arrived, so the alarm reports health
/// forever after the first tick of the morning — silently, which is the
/// failure direction that matters. `auth-failed-alarm.tf` records the same
/// uncertainty and lands on the opposite side of it (over-paging), and
/// `live-lane-alarms.tf` names this exact residual in its own header.
///
/// A GAUGE has no such ambiguity: both pipelines publish it verbatim, because
/// there is no delta to compute for a value that is free to go down. So the
/// question "is the feed alive" is asked of a signal that means the same thing
/// under either answer, instead of being decided by a coin flip nobody in this
/// repo has been able to call.
///
/// # What it measures
///
/// Feed-level liveness from [`FeedHealthRegistry`], which is stamped in
/// `flush_and_record` when a flush actually PERSISTS rows — not when a frame
/// is decoded and not when a row is appended to the ILP buffer. So a QuestDB
/// outage decays this value even while the socket is busy, which is correct:
/// during one, the feed genuinely is not delivering.
///
/// [`FeedHealthRegistry`]: tickvault_common::feed_health::FeedHealthRegistry
/// How long shutdown waits for the tick writer to finish its last batch.
///
/// Sized against the ILP client's own 5 s request timeout: one flush already
/// on the wire, plus the queue behind it, plus slack. Finite by requirement —
/// `JoinHandle::join` has no timeout, and a writer wedged on a hung socket
/// would otherwise hang a shutdown that the host's cost control depends on.
pub const OFFLOAD_SHUTDOWN_GRACE_SECS: u64 = 30;

/// [`OFFLOAD_SHUTDOWN_GRACE_SECS`] as a `Duration`.
pub const OFFLOAD_SHUTDOWN_GRACE: std::time::Duration =
    std::time::Duration::from_secs(OFFLOAD_SHUTDOWN_GRACE_SECS);

pub const LAST_TICK_AGE_GAUGE: &str = "tv_dhan_feed_last_tick_age_secs";

/// The value [`LAST_TICK_AGE_GAUGE`] publishes.
///
/// `age` is `None` until the first flush persists a row. Publishing 0 for that
/// case would read as perfect health during the exact outage the gauge exists
/// to catch — a lane that dials, connects, subscribes and never receives
/// anything is the 2026-08-12 shape, and it must page rather than reassure.
/// Publishing a magic sentinel instead would page correctly but tell a
/// dashboard reader nothing, so the never-ticked value is the drain's own
/// uptime: it grows for exactly as long as the silence has lasted, which is
/// the same thing the ticked branch reports, measured from the only other
/// moment that means anything here.
///
/// O(1), no allocation, and pure so the substitution above is testable rather
/// than asserted.
#[must_use]
pub fn last_tick_age_gauge_value(age: Option<u64>, drain_uptime_secs: u64) -> f64 {
    match age {
        Some(secs) => secs as f64,
        None => drain_uptime_secs as f64,
    }
}

/// How often the lane seals buckets the watermark has already moved past.
///
/// 5s is chosen against the SHORTEST timeframe the aggregator carries (1s):
/// a bar can be late by at most this interval plus the lateness margin, so
/// the 1s frame lands within seconds rather than at the 15:30 close sweep.
pub const CATCHUP_SEAL_INTERVAL_SECS: u64 = 5;

/// [`CATCHUP_SEAL_INTERVAL_SECS`] as a `Duration`.
const CATCHUP_SEAL_INTERVAL: std::time::Duration =
    std::time::Duration::from_secs(CATCHUP_SEAL_INTERVAL_SECS);

/// How far BEHIND the watermark the catch-up cutoff sits.
///
/// The watermark is the highest exchange timestamp across ALL instruments,
/// and ticks arrive out of order between them — so sealing exactly at the
/// watermark would close a bucket whose own last ticks are still in flight,
/// producing a truncated bar. That is strictly worse than the late bar this
/// mechanism exists to fix, so the margin is not optional.
///
/// 2s covers the observed inter-instrument reordering window with room to
/// spare, at the cost of 2s of extra latency on every catch-up bar.
const CATCHUP_LATENESS_MARGIN_SECS: u32 = 2;

/// How often the lane asks the gap detector what it has recorded.
///
/// 30s matches [`DEFAULT_SILENCE_FLOOR_MILLIS`] — scanning faster cannot
/// surface anything new, because nothing can be judged silent below that
/// floor. The scan is O(n) in tracked instruments, so it deliberately does
/// NOT ride the 500 ms flush timer.
pub const SILENCE_SCAN_INTERVAL_SECS: u64 = 30;

/// [`SILENCE_SCAN_INTERVAL_SECS`] as a `Duration`.
const SILENCE_SCAN_INTERVAL: std::time::Duration =
    std::time::Duration::from_secs(SILENCE_SCAN_INTERVAL_SECS);

/// Consecutive silence scans that must agree before the lane pages.
///
/// One scan is not evidence: a scan landing in the shadow of a reconnect, or
/// during the first seconds after a subscribe batch, sees instruments that
/// are legitimately not ticking YET. Two consecutive scans 30s apart mean the
/// condition survived a full detector cycle.
const SILENCE_SCANS_BEFORE_ALERT: u32 = 2;

/// How many frames pass before the fold republishes its depth gauges.
const DRAIN_REPORT_EVERY: u64 = 1_024;

/// Drains captured frames into the fold, forever.
///
/// This is the ONE consumer of the ring, and it runs on its own task for a
/// reason the read loop's docs spell out: parsing on the read task is what
/// stops the pong flowing and turns a slow fold into a disconnect. Here, a
/// slow fold shows up as ring pressure (`tv_dhan_ws_ring_full_total`) against
/// a WAL that already holds every frame.
///
/// # Complexity
///
/// **O(1) per PACKET, O(packets) per FRAME.** Per packet: one fixed-offset
/// parse, one hash lookup in the gap detector, one hash lookup plus
/// `TF_COUNT` scalar folds in the aggregator, one ILP row append. No heap
/// allocation in steady state.
///
/// The per-FRAME cost is NOT constant, and this comment said it was until
/// 2026-08-14. A main-feed message may carry several stacked packets —
/// `drain_main_feed_frame` walks them in a `while offset < frame.bytes.len()`
/// loop bounded by `MAX_PACKETS_PER_FRAME` — so worst-case frame cost is that
/// bound times the per-packet cost, not one packet's worth. Calling that O(1)
/// understated the worst case by orders of magnitude, which is exactly the
/// kind of claim CLAUDE.md's complexity table exists to stop us making.
/// Typical frames carry one packet per subscribed instrument on that socket.
/// Run the SYNCHRONOUS blocking ILP-over-HTTP flush OFF the async worker
/// (the `order_observability::blocking_flush` house pattern — the same shape
/// is inlined at `seal_writer_loop.rs`, `groww_order_observability.rs`,
/// `dhan_order_push_observability.rs`, `order_update_events_boot.rs` and
/// `cadence_escalation.rs`).
///
/// `TickWriter::flush` is a blocking HTTP call bounded by the conf-pinned
/// `request_timeout=5000`. Called bare on this task it pins a tokio worker;
/// on a 2-worker host that is HALF the runtime, and the worker it pins is
/// shared with the WS read loops — which stop pumping pongs and get the
/// socket dropped. Worse, the drain stops draining, so the 65,536-frame ring
/// fills in ~13 s at 5,000 fps and every frame after that is refused. Those
/// frames reach the WAL, but the WAL has no re-fold path, so they never
/// become rows. A third-party database stall therefore became permanent tick
/// loss plus a disconnect — via a task that had no business blocking at all.
///
/// The runtime-flavor guard is MANDATORY, not stylistic: `block_in_place`
/// panics on a current-thread runtime, and this module's drain tests are bare
/// `#[tokio::test]` (current_thread).
///
/// `block_in_place` rather than `spawn_blocking` because the closure borrows
/// `&mut ingest` — no `'static`/`Send` bound, no channel round-trip — and it
/// converts the CURRENT worker into a blocking thread while the runtime spins
/// up a replacement, so the effective worker count is preserved.
fn blocking_flush<T>(flush: impl FnOnce() -> T) -> T {
    if tokio::runtime::Handle::current().runtime_flavor()
        == tokio::runtime::RuntimeFlavor::MultiThread
    {
        tokio::task::block_in_place(flush)
    } else {
        flush()
    }
}

/// Flush the ILP buffer and tell feed-health how many rows actually landed.
///
/// The two halves live in one function because separating them is what went
/// wrong. Before 2026-08-18 the drain called `ingest.flush()` at three
/// steady-state sites and DISCARDED the returned row count at every one —
/// the number was computed and dropped. Meanwhile `record_ticks` had ZERO
/// production callers anywhere in the workspace, so the Dhan lane could pump
/// rows all session and `feed_health` still answered
/// `Unknown — "not instrumented yet"`. A dead lane and a healthy one produced
/// the identical verdict, on the one signal an operator checks to find out
/// which it is.
///
/// **Rows FLUSHED is the deliberate unit** — not frames received, not rows
/// buffered:
///
/// * A frame that arrived and failed to parse must not make the feed look
///   alive; nothing reached the database.
/// * A row still in the ILP buffer is not readable by anything. Counting a
///   buffer append as liveness would report health for data a crash takes
///   with it.
/// * A FAILED flush returns 0 by `TickWriter` contract, so it records nothing
///   and health decays — which is correct. During a QuestDB outage the feed
///   genuinely is not delivering, however busy the socket looks.
///
/// `record_ticks` is a no-op at `n == 0` and deliberately does not stamp the
/// time in that case, so the 500 ms idle timer cannot forge liveness for an
/// instrument set that never ticked.
///
/// Cost: two relaxed atomic stores plus one clock read, at FLUSH cadence
/// (500 ms timer or the row threshold) — never per tick.
fn flush_and_record(
    ingest: &mut LiveIngest,
    feed_health: &tickvault_common::feed_health::FeedHealthRegistry,
) -> u64 {
    // OFFLOADED: health is NOT recorded here. `rows` on this path means rows
    // HANDED OFF, and health is defined three paragraphs up as rows FLUSHED.
    // Recording on hand-off would forge liveness for exactly as long as the
    // database was unreachable: the queue accepts batches, the sink fails
    // every write, and the one signal an operator checks reads green. The
    // writer thread records instead, from the rows that actually landed —
    // see `LiveIngest::spawn_offload_writer`.
    //
    // CORRECTED 2026-08-26 — the sentence that opened this comment used to
    // read "the flush is a bounded-queue hand-off with no network in it, so
    // there is nothing to move off-worker", and it returned `ingest.flush()`
    // BARE on the strength of that. It was half true and the half it missed
    // is the one with the network in it: `LiveIngest::flush` flushes the
    // inline-depth sink FIRST and unconditionally (see its own body, which
    // says so and explains why it sits above the early return), and
    // `DepthIngest::flush` is a blocking ILP-over-HTTP round trip bounded by
    // the conf-pinned `request_timeout=5000`.
    //
    // So the offload — landed precisely to get blocking HTTP off this task —
    // left a blocking HTTP call on this task, on the ONLY path production
    // takes. Production enables both halves: `spawn_offload_writer` at boot
    // and `with_inline_depth` on the same builder chain, so
    // `writer_is_offloaded()` is true and `inline_depth` is `Some` on every
    // real run, and this early return was reached every flush.
    //
    // The consequence is the mechanism `blocking_flush`'s own docs describe:
    // a stalled QuestDB pins a tokio worker for up to 5 s, the WS read loops
    // sharing that worker stop pumping pongs, the drain stops draining, and
    // the ring fills. It is not merely slow — it is a tick-loss and
    // disconnect path, which is the whole reason the helper exists.
    //
    // Wrapped UNCONDITIONALLY rather than gated on "is there depth pending":
    // a gate needs a second accessor that must be kept in step with what
    // `LiveIngest::flush` actually does, and the last time these two pieces
    // of knowledge were kept in separate places is what produced this bug.
    // The cost of being wrong the cheap way is one worker swap at ~5 flushes
    // per second; the cost of being wrong the other way is this comment.
    if ingest.writer_is_offloaded() {
        return blocking_flush(|| ingest.flush());
    }
    let rows = blocking_flush(|| ingest.flush());
    feed_health.record_ticks(
        Feed::Dhan,
        rows,
        chrono::Utc::now()
            .timestamp_nanos_opt()
            .unwrap_or(0)
            .saturating_add(tickvault_common::constants::IST_UTC_OFFSET_NANOS),
    );
    rows
}

/// Has a RISK-GAP-03 page fired recently enough to suppress the next one?
///
/// Extracted as a pure function rather than written inline because the inline
/// form was only checkable by a source scan, and a source scan for a literal
/// is defeated by the same blanket edit that would break the code — a first
/// draft of that guard rewrote the assertion and the call site together and
/// passed. Behaviour is testable; text is not.
///
/// Returns `false` when no page has fired yet, so the FIRST page of a session
/// is never suppressed.
///
/// # Clock, not counter
///
/// Both arguments are seconds. The call site sits a few lines below a binding
/// named `now` that holds `ingest.refusals()` — a counter — so passing the
/// wrong one compiles and produces a cooldown that is either permanent or
/// inert depending on how many refusals happened to have occurred.
/// `test_a_counter_shaped_value_does_not_silently_work_as_a_clock` pins the
/// consequence.
fn silence_page_is_cooling(last_page_secs: Option<u64>, now_secs: u64, cooldown: u64) -> bool {
    // saturating_sub, because a clock that steps backwards (NTP correction)
    // must not wrap into a gigantic elapsed value and silently clear the
    // cooldown at the one moment the log is hardest to read.
    last_page_secs.is_some_and(|last| now_secs.saturating_sub(last) < cooldown)
}

/// Flushes the depth ILP buffer, if a depth ingest is wired.
///
/// Deliberately a separate call from the tick flush rather than folded into
/// it: the two writers hold independent buffers over independent tables, and
/// a tick-flush failure must not skip the depth flush (nor the reverse). The
/// writer already discards-and-logs its own failure, so the failure is loud at
/// its source and this wrapper does not need to re-report it.
fn flush_depth(depth: Option<&mut DepthIngest>) {
    let Some(depth) = depth else { return };
    if depth.pending_rows() == 0 {
        return;
    }
    // The writer counts its own discards, but that counter lives in the
    // storage crate and is NOT the EMF-shipped name. Mirroring the DELTA into
    // the labelled drain counter is what makes a database-side depth loss
    // visible in CloudWatch at all — and the delta, not the total, because a
    // cumulative would read alarming forever after one bad flush.
    let before = depth.dropped_rows();
    if let Err(err) = blocking_flush(|| depth.flush()) {
        // Deliberately `debug!`, not a second `error!`: the writer already
        // logged this failure at ERROR with the discarded row count, and
        // re-reporting it here would double every depth flush failure in the
        // log while adding nothing an operator can act on.
        tracing::debug!(
            ?err,
            "market_depth flush failed (already reported by the writer)"
        );
    }
    let delta = depth.dropped_rows().saturating_sub(before);
    if delta > 0 {
        counters().depth_dropped.increment(delta);
    }
}

async fn run_frame_drain(
    mut rx: tokio::sync::mpsc::Receiver<CapturedFrame>,
    mut ingest: LiveIngest,
    mut depth_ingest: Option<DepthIngest>,
    main_feed_budget: Arc<RingByteBudget>,
    depth_budget: Arc<RingByteBudget>,
    shutdown: Arc<tokio::sync::Notify>,
    feed_health: Arc<tickvault_common::feed_health::FeedHealthRegistry>,
    // Instruments that attach AFTER boot — the ~20,000 futures and option
    // contracts the late-attach pass dials once a price exists.
    //
    // They need a channel because by the time they are selected, the ingest
    // belongs to THIS task. Before this existed, only the boot spot universe
    // was ever seeded, so a contract that subscribed and then silently
    // delivered nothing was invisible to the gap detector: there is no
    // payload to count, no parse to fail and no error to log, so absence
    // against a seeded key is the ONLY evidence that exists. Roughly 80% of
    // the authorized universe had no silence detection at all.
    mut seed_rx: tokio::sync::mpsc::Receiver<Vec<SubscribeInstrument>>,
) -> DrainOutcome {
    // Whether this drain has ever seen a frame. Owns the up-gauge's rising
    // edge — see the first-frame arm below and the spawn site's correction.
    let mut lane_up = false;
    // Anchor for the never-ticked branch of `LAST_TICK_AGE_GAUGE`. Taken here
    // rather than at the stack's boot so it measures the drain's OWN silence:
    // the drain is the thing that would be reporting ticks, and dialing time
    // before it exists is not silence anyone can act on.
    let drain_started = std::time::Instant::now();
    let mut seen: u64 = 0;
    let mut folded: u64 = 0;
    let mut depth_unconsumed: u64 = 0;
    let mut depth_rows: u64 = 0;
    let mut depth_refused: u64 = 0;
    let mut unparseable: u64 = 0;
    let mut flush_timer = tokio::time::interval(FLUSH_INTERVAL);
    flush_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Mid-session catch-up seal. Without it, a bar for an instrument that
    // stops ticking waits for the 15:30 close sweep — correct, but hours late.
    let mut catchup_timer = tokio::time::interval(CATCHUP_SEAL_INTERVAL);
    catchup_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Deliberately its OWN timer rather than a counter on the flush arm: the
    // scan is O(n) in tracked instruments and the flush arm runs at 500 ms.
    let mut silence_timer = tokio::time::interval(SILENCE_SCAN_INTERVAL);
    silence_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Consecutive alarm-worthy scans, and whether we have already paged for
    // this episode. Edge-triggered per audit Rule 4: the rising edge fires
    // once, the falling edge logs recovery at info and re-arms.
    let mut silent_scans: u32 = 0;
    let mut silence_reported = false;
    // Latch for the detector-blindness report below. Once the slot table is
    // full it stays full, so this fires once per session rather than every 30s.
    let mut detector_blind_reported = false;
    // Caller-owned storage for the named silent instruments, so the 30s scan
    // stays allocation-free. Reused every scan; `named` bounds what is read.
    let mut worst_silent = [SilentInstrument::EMPTY; WORST_SILENT_NAMED];
    /// Shortest gap between two RISK-GAP-03 pages, however many separate
    /// silence episodes occur inside it. See `last_silence_page` below.
    const SILENCE_PAGE_COOLDOWN_SECS: u64 = 1_800;
    let mut last_silence_page: Option<u64> = None;
    // Last reported aggregator-refusal totals, so the 30s arm can report a
    // DELTA rather than a cumulative that looks alarming forever after one
    // bad minute.
    let mut last_refusals: (u64, u64, u64, u64) = (0, 0, 0, 0);

    loop {
        tokio::select! {
            // Biased so frames always win a tie: the flush timer firing while
            // frames are queued must not preempt draining them.
            biased;
            maybe_frame = rx.recv() => {
                let Some(frame) = maybe_frame else { break };
                // The lane is UP the moment a frame actually arrives, and not
                // one instant sooner (plan Item 7). Raised once per drain, from
                // the only place in the process that has proof a socket both
                // connected AND delivered — the bytes are in hand.
                //
                // A gauge rather than a counter, and set unconditionally on the
                // first frame rather than tracked with an edge, because the
                // drain is the single consumer: there is no second writer to
                // race, and re-setting an already-1.0 gauge is free.
                if !lane_up {
                    lane_up = true;
                    metrics::gauge!(FEED_STACK_UP_GAUGE).set(1.0);
                    info!(
                        "Dhan live lane is UP — first frame received and consumed by the fold"
                    );
                }
                // Release the byte reservation the instant the frame leaves the
                // ring — BEFORE any parsing, and on EVERY path out of this arm.
                // Releasing after a successful parse instead would leak the
                // budget for every unparseable or depth-unconsumed frame, and
                // those are exactly the frames a degraded feed produces most:
                // the bound would tighten precisely when the feed most needs
                // head-room, and the process would strangle itself with what is
                // meant to protect it.
                //
                // Released to the SAME budget that reserved it, chosen by the
                // frame's own endpoint rather than by position. Releasing to
                // the wrong pool would be worse than not splitting at all: one
                // budget would drift permanently full while the other drifted
                // negative-clamped to zero, so the feed would refuse frames it
                // had head-room for and admit frames it did not.
                match frame.endpoint {
                    DhanEndpointType::MainFeed => main_feed_budget.release(frame.bytes.len()),
                    _ => depth_budget.release(frame.bytes.len()),
                }
                seen = seen.saturating_add(1);
                let c = counters();
                // The frame's OWN receipt instant, reconstructed from the
                // monotonic stamp the read task took when it came off the
                // socket — NOT a fresh `Utc::now()` here.
                //
                // Until 2026-08-18 this bound `Utc::now()` directly, which
                // made every lag sample
                // `Dhan's delivery + however long the frame sat in the ring`.
                // Those are different quantities with different owners: the
                // first is the vendor's, the second is ours. Under a fold
                // stall the ring backs up and the drain falls behind, so the
                // measurement inflated precisely when the cause was LOCAL —
                // the lag alarm would have fired hardest at our own backlog
                // and named Dhan for it.
                //
                // Subtracting `elapsed()` rather than carrying a wall-clock
                // stamp on the frame keeps `pool_supervisor` free of the wall
                // clock (an NTP step must not be able to expire all sixteen
                // sockets at once), and is strictly more robust: a clock step
                // landing between receipt and fold cancels out of the
                // difference instead of corrupting the sample.
                let queued_nanos =
                    i64::try_from(frame.received_at.elapsed().as_nanos()).unwrap_or(i64::MAX);
                // Publish what was already being computed. Until 2026-08-26
                // this value was derived on EVERY frame — ~5,000 times a
                // second — used once to back-date `received_at_nanos`, and
                // then dropped, so the one number that says how far behind our
                // own drain is existed for a microsecond and reached nothing.
                record_ring_dwell(queued_nanos);
                let received_at_nanos = chrono::Utc::now()
                    .timestamp_nanos_opt()
                    .unwrap_or(0)
                    .saturating_sub(queued_nanos);
                // The gap detector's clock is a millisecond reading; the same
                // wall-clock instant is used so a frame's arrival and its
                // silence-accounting can never disagree.
                let recv_millis = u64::try_from(received_at_nanos / 1_000_000).unwrap_or(0);
                // The same instant as an `i64`, for the per-connection
                // delivery stamp. Derived from `received_at_nanos` rather than
                // taken fresh so a socket's stamp and its silence-accounting
                // can never disagree by a scheduling delay — the same reason
                // `recv_millis` above exists.
                let recv_millis_i64 = received_at_nanos / 1_000_000;

                // Routed BY ENDPOINT, never by guesswork. Depth frames carry a
                // 12-byte header and can stack several packets in one message;
                // feeding one to the main-feed dispatcher makes byte 0 a length
                // low-byte that matches no response code, so every depth frame
                // would be counted "unparseable" and silently lost.
                match frame.endpoint {
                    DhanEndpointType::MainFeed => {
                        let outcome = drain_main_feed_frame(
                            &mut ingest, &frame, received_at_nanos, recv_millis, c,
                        );
                        folded = folded.saturating_add(outcome.folded);
                        unparseable = unparseable.saturating_add(outcome.unparseable);
                        // Stamp the SOCKET as delivering, gated on the frame
                        // having actually produced something.
                        //
                        // Gated, not unconditional: a socket returning frames
                        // that all fail to parse is not delivering market
                        // data, and stamping on arrival would report it
                        // healthy — the precise false-OK this detector exists
                        // to remove. `folded > 0` is the honest bar.
                        if outcome.folded > 0 {
                            record_connection_tick(frame.connection_index, recv_millis_i64);
                        }
                    }
                    DhanEndpointType::Depth20 | DhanEndpointType::Depth200 => {
                        // PERSISTED since 2026-08-15 (operator: depth-20 and
                        // depth-200 "shwon and vsisibil in one common atbek …
                        // we cnanot miss or hdi or wipe fof nayhtign").
                        //
                        // Until then this arm counted the frame as
                        // `depth_unconsumed` and dropped it — captured durably
                        // in the WAL and then discarded, which is still a
                        // discard. The counter is KEPT and now means something
                        // narrower and more useful: a depth frame that reached
                        // the drain with NO depth ingest wired, which can only
                        // happen if the stack was built without one. Zero in
                        // normal operation; non-zero is a wiring bug, not a
                        // design choice, and it must not read as one.
                        let kind = if frame.endpoint == DhanEndpointType::Depth20 {
                            DepthFeedKind::Twenty
                        } else {
                            DepthFeedKind::TwoHundred
                        };
                        match depth_ingest.as_mut() {
                            // The ingest-shed gate, read here rather than
                            // inside `drain_depth_frame`, so a shed frame
                            // costs one relaxed atomic load and NOT a parse.
                            // The frame is already durable in the WAL by this
                            // point — shedding drops the DATABASE write, never
                            // the capture.
                            Some(_) if !INGEST_SHED.allows_dedicated_depth() => {
                                c.shed_dedicated_depth.increment(1);
                            }
                            Some(depth) => {
                                let outcome = drain_depth_frame(
                                    depth, &frame, received_at_nanos, kind, c,
                                );
                                depth_rows = depth_rows.saturating_add(outcome.rows);
                                // Depth sockets get the same delivery stamp as
                                // the main feed. Without this every depth
                                // connection reads "never ticked" and is
                                // excluded from the worst-age gauge — which
                                // would leave ten of the sixteen sockets with
                                // no deaf-socket coverage at all, while the
                                // gauge looked healthy for that very reason.
                                if outcome.rows > 0 {
                                    record_connection_tick(
                                        frame.connection_index,
                                        recv_millis_i64,
                                    );
                                }
                                depth_refused = depth_refused.saturating_add(outcome.refused);
                            }
                            None => {
                                depth_unconsumed = depth_unconsumed.saturating_add(1);
                                c.depth_unconsumed.increment(1);
                            }
                        }
                    }
                    DhanEndpointType::OrderUpdate => c.non_tick.increment(1),
                }

                // SIZE trigger. Rows sitting in the ILP buffer have NOT reached
                // QuestDB — an unflushed buffer is not storage, it is a leak
                // with a success counter in front of it.
                if ingest.pending_rows() >= FLUSH_ROW_THRESHOLD {
                    flush_and_record(&mut ingest, &feed_health);
                }
                // Depth gets its OWN size trigger — see
                // `DEPTH_FLUSH_ROW_THRESHOLD` for why reusing the tick one
                // would have turned the drain into a synchronous HTTP loop.
                if depth_ingest.as_ref().is_some_and(|d| {
                    u64::try_from(d.pending_rows()).unwrap_or(u64::MAX) >= DEPTH_FLUSH_ROW_THRESHOLD
                }) {
                    flush_depth(depth_ingest.as_mut());
                }
                if seen.is_multiple_of(DRAIN_REPORT_EVERY) {
                    publish_fold_depth(&ingest);
                }
            }
            // SHUTDOWN (added 2026-08-14). Deliberately placed BELOW the frame
            // arm in this `biased` select, and that ordering is load-bearing:
            // a shutdown signal must not preempt frames already sitting in the
            // ring. Placed above, the permit wins the very first poll and the
            // drain exits abandoning queued work — which is a different way of
            // losing the tail than the bug this arm exists to fix. The test
            // `test_drain_exits_on_shutdown_signal_with_the_ring_still_open`
            // caught exactly that during development, which is why it asserts
            // the queued frame was FOLDED and not merely that the drain ended.
            //
            // Before this arm existed the drain
            // could only end when the ring closed, and nothing closed it: the
            // lane's handle was bound to `_dhan_feed_stack_monitor` and the
            // shutdown path's Dhan steps had been "deleted with the lane" in
            // 2026-07-13 — then the lane came back and the teardown did not.
            //
            // The consequence was silent and daily. At the 17:30 stop, SIGTERM
            // ran the process teardown, `main` returned `Ok(())`, and the log
            // printed "tickvault stopped" and classified the shutdown clean —
            // while every ILP row still under FLUSH_ROW_THRESHOLD and every
            // open candle in the aggregator went with the process. There was
            // no metric whose value differed between a day that flushed and a
            // day that did not.
            //
            // `Notify::notify_one` is permit-based, so a signal that arrives
            // while this task is inside another arm is retained rather than
            // lost — the lost-wake hazard that makes `notify_waiters` the
            // wrong primitive here (audit-findings Rule 16).
            () = shutdown.notified() => {
                info!("Dhan live feed: shutdown signalled — sealing and flushing before exit");
                // Seal whatever the aggregator still holds, THEN flush, in
                // that order: sealing produces rows, so flushing first would
                // leave exactly the rows sealing just created.
                let (emitted, dropped) = ingest.catch_up_seal();
                flush_and_record(&mut ingest, &feed_health);
                flush_depth(depth_ingest.as_mut());
                if dropped > 0 {
                    error!(
                        code = ErrorCode::WsGapConnectionState.code_str(),
                        emitted,
                        dropped,
                        "Dhan live feed: candles were DROPPED during the shutdown seal — the \
                         seal ring could not take them and they are lost with the process"
                    );
                } else {
                    info!(
                        emitted,
                        "Dhan live feed: shutdown seal + flush complete — the day's tail is \
                         persisted"
                    );
                }
                break;
            }
            // TIME trigger. Without it, the last rows of a thinly-traded
            // instrument sit unflushed below the size threshold waiting for a
            // next tick which, at the close, never comes.
            _ = flush_timer.tick() => {
                flush_and_record(&mut ingest, &feed_health);
                flush_depth(depth_ingest.as_mut());
                publish_fold_depth(&ingest);
            }
            // Mid-session catch-up seal. Deliberately BEFORE the silence arm
            // in source order but with no `biased` dependency between them —
            // they touch disjoint state (aggregator vs detector) and neither
            // starves the other, because both are timers, not a queue.
            _ = catchup_timer.tick() => {
                let (emitted, dropped) = ingest.catch_up_seal();
                if emitted > 0 || dropped > 0 {
                    publish_fold_depth(&ingest);
                }
                if dropped > 0 {
                    // Same class as a per-tick seal drop: the candle was
                    // computed and thrown away. Counted in the shared totals
                    // by `catch_up_seal`; named here so the operator learns
                    // WHICH path lost it.
                    warn!(
                        code = ErrorCode::WsGapConnectionState.code_str(),
                        emitted,
                        dropped,
                        "catch-up seal dropped {dropped} computed candle(s) — the seal \
                         writer was absent or its queue was full"
                    );
                }
            }
            // The detector's read-out. Until 2026-08-12 the lane fed this
            // detector on every tick and never asked it anything, so a
            // subscribe that silently did not take produced no signal at all.
            // Seed instruments that attached after boot. Placed in the drain
            // because the drain owns the ingest, and NOT biased ahead of
            // frames: seeding is bookkeeping, and a queued frame is data.
            //
            // Deliberately seeds on the SUBSCRIBE, not on the first tick. A
            // detector that learned instruments from arriving ticks could
            // never report the one failure that matters here — an instrument
            // that arrives never.
            maybe_seed = seed_rx.recv() => {
                if let Some(batch) = maybe_seed {
                    let now_millis = u64::try_from(
                        chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0).max(0) / 1_000_000,
                    )
                    .unwrap_or(0);
                    // `seed` returns whether the slot allocator ACCEPTED the
                    // instrument, not whether it was new -- a repeat seed is a
                    // successful no-op. So this counts accepted, and a value
                    // below `requested` means slots ran out.
                    let mut added = 0usize;
                    for inst in &batch {
                        if ingest.seed(inst.security_id, inst.segment, now_millis) {
                            added = added.saturating_add(1);
                        }
                    }
                    info!(
                        requested = batch.len(),
                        seeded = added,
                        tracked = ingest.tracked_instruments(),
                        "late-attached instruments seeded into the silence detector"
                    );
                }
                // A closed channel is NORMAL — the attach task finishes and
                // drops its sender. `recv` then returns None forever, so this
                // arm must not treat that as a reason to end the drain.
            }
            _ = silence_timer.tick() => {
                let now_millis = u64::try_from(
                    chrono::Utc::now().timestamp_millis().max(0)
                ).unwrap_or(0);

                // Aggregator-refusal read-out. The refusal arm itself cannot
                // log (per-tick path, flood risk), so this is where those
                // counts become visible. Reported BEFORE the silence gate's
                // market-hours `continue` below, because a refusal is a
                // defect at any hour — unlike silence, which is normal after
                // the close.
                let now = ingest.refusals();
                let d_price = now.0.saturating_sub(last_refusals.0);
                let d_ts = now.1.saturating_sub(last_refusals.1);
                let d_slot = now.2.saturating_sub(last_refusals.2);
                // `out_of_session` (now.3) is deliberately NOT reported: it is
                // the designed refusal for a tick outside the fold window, and
                // folding it in here would page for normal pre-open traffic —
                // exactly the false-alarm class the silence gate was fixed for.
                if d_price > 0 || d_ts > 0 || d_slot > 0 {
                    error!(
                        code = ErrorCode::AggregatorDrop01.code_str(),
                        refused_price = d_price,
                        refused_timestamp = d_ts,
                        refused_slot_exhausted = d_slot,
                        "Dhan live feed: the aggregator refused ticks in the last 30s. \
                         These ticks were NOT folded into any candle and NOT written. \
                         A price or timestamp refusal means the upstream packet failed \
                         a sanity check; a slot refusal means the instrument capacity \
                         is exhausted and NEW instruments are being turned away."
                    );
                    last_refusals = now;
                }

                let (silent, never, named) =
                    ingest.scan_silence_named(now_millis, &mut worst_silent);

                // The detector's own blindness, reported ONCE per episode.
                //
                // The gauge inside `scan_silence` publishes this every scan,
                // but the gauge does not reach CloudWatch: it did not fit the
                // EC2 user-data byte budget (see EMF-METRIC-SELECTOR-NOTES.md).
                // A counter that measures a blind spot and reaches nobody is
                // worse than no counter — the loss is measured, the
                // measurement is discarded, and the dashboard stays green — so
                // the coded log line is what carries it to the operator.
                //
                // Edge-latched rather than per-scan: once the slot table is
                // full it stays full, so an unlatched line would repeat every
                // 30 seconds for the rest of the session and bury everything
                // else in the sink.
                let refused_now = ingest.detector_refused();
                metrics::gauge!(SILENCE_DETECTOR_REFUSED_GAUGE).set(refused_now as f64);
                if refused_now > 0 && !detector_blind_reported {
                    detector_blind_reported = true;
                    error!(
                        code = ErrorCode::RiskGapTickGap.code_str(),
                        refused = refused_now,
                        tracked = ingest.tracked_instruments(),
                        "the silence detector ran out of slots and is now BLIND to \
                         part of the universe. Its silent and never-ticked counts \
                         describe only the instruments it still tracks, while \
                         reading exactly as though they describe all of them — so a \
                         silently-unsubscribed instrument among the refused ones \
                         cannot be reported by anything. Raise the detector's \
                         capacity or reduce the subscribed set."
                    );
                }

                // Feed-level liveness, published UNCONDITIONALLY and before
                // the market-hours gate below — same reasoning as the two
                // gauges `scan_silence` sets: a gauge that stops publishing
                // makes "no data" and "nothing wrong" indistinguishable, and
                // this is the one signal whose whole job is telling those two
                // apart.
                //
                // Reading the registry rather than a local counter is
                // deliberate: `flush_and_record` stamps it only when a flush
                // PERSISTED rows, so this decays during a QuestDB outage even
                // while the socket is busy. That is the honest answer to "is
                // the feed delivering".
                metrics::gauge!(LAST_TICK_AGE_GAUGE).set(last_tick_age_gauge_value(
                    feed_health.last_tick_age_secs(
                        Feed::Dhan,
                        chrono::Utc::now()
                            .timestamp_nanos_opt()
                            .unwrap_or(0)
                            .saturating_add(tickvault_common::constants::IST_UTC_OFFSET_NANOS),
                    ),
                    drain_started.elapsed().as_secs(),
                ));
                // The PER-SOCKET half of the same question, published on the
                // same tick and for the same reason.
                //
                // The lane gauge above cannot see one deaf socket among
                // sixteen: with fifteen delivering, the lane's last tick is
                // always a second old however dead the sixteenth is. This one
                // reports the WORST socket, so a single deaf connection moves
                // it while the lane gauge stays flat — and that difference IS
                // the diagnosis.
                //
                // -1 for "no connection has ticked yet", which is a real state
                // (pre-open, or a lane that just started) and must not render
                // as 0. Zero would read as "every socket ticked this instant",
                // the most reassuring value available, at the moment we know
                // least.
                metrics::gauge!(WORST_CONN_TICK_AGE_GAUGE).set(
                    worst_connection_tick_age_secs(
                        chrono::Utc::now().timestamp_millis(),
                    )
                    .map_or(-1.0, |secs| f64::from(u32::try_from(secs).unwrap_or(u32::MAX))),
                );
                // Ring OCCUPANCY, the companion to the dwell gauge above.
                //
                // Published here rather than in `publish_fold_depth` because
                // the budgets live on the drain's own frame, not on the
                // ingest, and threading them through that function's signature
                // would touch four call sites to move two atomic loads.
                //
                // Per-pool detail is published unselected alongside the
                // worst-of-two: labels are free on `/metrics` and cost a
                // CloudWatch dimension each, so the pair that answers "is the
                // ring filling?" ships and the pair that answers "which one?"
                // stays local, where whoever is triaging is already looking.
                let main_pct =
                    budget_fill_pct(main_feed_budget.resident(), main_feed_budget.cap());
                let depth_pct = budget_fill_pct(depth_budget.resident(), depth_budget.cap());
                metrics::gauge!(RING_RESIDENT_PCT_GAUGE).set(main_pct.max(depth_pct));
                metrics::gauge!("tv_dhan_feed_ring_resident_pct_by_pool", "pool" => "main_feed")
                    .set(main_pct);
                metrics::gauge!("tv_dhan_feed_ring_resident_pct_by_pool", "pool" => "depth")
                    .set(depth_pct);
                // Gauges publish unconditionally — a dashboard reading zero
                // outside market hours is correct, and gating the gauge would
                // make "no data" and "nothing wrong" indistinguishable.
                // Only the PAGE is market-hours gated (audit Rule 3): the
                // whole universe is legitimately silent after 15:30, and a
                // detector that pages every evening gets muted by lunchtime.
                //
                // The TRADING-DAY half of that gate is just as load-bearing and
                // was missing until 2026-08-14. EventBridge starts this box on
                // `MON-FRI`, which includes NSE holidays, so on a weekday
                // holiday the lane seeds the whole universe, receives nothing —
                // correctly, the market is shut — and every seeded instrument
                // crosses the silence floor at once. The page that follows says
                // `silent=<universe>, never_ticked=<universe>`, which is
                // indistinguishable from a total subscribe failure. Every
                // sibling leg in this repo already gates on the calendar
                // (`groww_contract_1m_boot`, `groww_option_chain_1m_boot`,
                // `brutex_crossverify_boot`, `feed_scoreboard_boot`); the
                // revived lane was the one regression.
                if !is_within_market_hours_ist(now_ist_secs_of_day())
                    || !silence_page_allowed_today()
                {
                    silent_scans = 0;
                    silence_reported = false;
                    continue;
                }
                if silent == 0 {
                    if silence_reported {
                        info!(
                            "Dhan live feed: every tracked instrument is ticking again \
                             within its own expected cadence"
                        );
                    }
                    silent_scans = 0;
                    // Re-arm only once nothing is left in the never-ticked
                    // set. `never_ticked` is one-way within a session — an
                    // instrument leaves it only by producing something, which
                    // also removes it from `silent` — so re-paging while it is
                    // non-zero restates a fact that cannot have changed.
                    //
                    // This gate alone is NOT what stops the page storm; see
                    // the cooldown at the emit below, and the measured numbers
                    // recorded there.
                    if never == 0 {
                        silence_reported = false;
                    }
                    continue;
                }
                silent_scans = silent_scans.saturating_add(1);
                // A cooldown between PAGES, not between episodes.
                //
                // # What the 2026-08-14 session actually did
                //
                // 25 distinct RISK-GAP-03 emits in one trading day. The
                // `silent` count oscillated the whole time — 4, 9, 1, 2, 1, 3,
                // 208, 10 — clearing to zero between episodes and re-arming
                // the latch each time, entirely legitimately: these are
                // sparse-cadence instruments going quiet and coming back.
                //
                // So the per-episode latch was working exactly as designed and
                // still produced ~25 pages, because the real world produced ~25
                // episodes. `never_ticked` was 4 on the 09:15 emit and 0 on
                // every one after it, so the feed WAS delivering — gating on
                // never-ticked alone (above) would have suppressed almost none
                // of this.
                //
                // That became a paging problem on 2026-08-15, when RISK-GAP-03
                // gained a CloudWatch alarm: at a 5-minute window with a
                // recovery page, 25 episodes is ~50 operator messages a day.
                // Half an hour is chosen to sit above the observed inter-episode
                // gap (two to five minutes through the afternoon) while staying
                // far below the session, so a genuinely new problem hours later
                // still pages.
                //
                // The counter is deliberately NOT gated — every episode still
                // increments `tv_dhan_feed_instruments_silent`, so the
                // suppressed ones remain countable on the dashboard. Only the
                // page is rate-limited, and the log line says how many were
                // folded in.
                // `now_millis`, not `now` — the latter is bound to
                // `ingest.refusals()` a few lines above, which is a COUNTER.
                // Dividing it by 1,000 compiles and yields a plausible-looking
                // small number that has nothing to do with time.
                let now_secs = now_millis / 1_000;
                let cooling =
                    silence_page_is_cooling(last_silence_page, now_secs, SILENCE_PAGE_COOLDOWN_SECS);
                if silent_scans >= SILENCE_SCANS_BEFORE_ALERT && !silence_reported && !cooling {
                    silence_reported = true;
                    last_silence_page = Some(now_secs);
                    error!(
                        code = ErrorCode::RiskGapTickGap.code_str(),
                        silent,
                        never_ticked = never,
                        tracked = ingest.tracked_instruments(),
                        consecutive_scans = silent_scans,
                        "Dhan live feed: {silent} subscribed instrument(s) are quiet beyond \
                         their own learned cadence across {silent_scans} consecutive 30s scans, \
                         of which {never} have produced NOTHING since being subscribed. A \
                         never-ticked instrument usually means its subscribe did not take — \
                         there is no other signal for that, because a stream that never \
                         arrives leaves nothing to count or fail to parse. \
                         Legitimately-sparse instruments are excluded from this count."
                    );
                    // NAME them. The line above says how many; these say WHICH.
                    //
                    // Until 2026-08-21 no `security_id` reached any log, metric
                    // or table on this path, so "which instruments went silent
                    // yesterday?" had no answer anywhere in the system — the
                    // information was computed, counted, and thrown away. It is
                    // also the one failure with no other evidence: a stream
                    // that never arrives leaves nothing to count and nothing to
                    // fail to parse, so absence against a seeded key is all
                    // there is.
                    //
                    // One line per instrument rather than one line listing
                    // them: each is greppable by `security_id`, each carries
                    // its own cadence context, and no line needs a formatted
                    // collection — so this stays allocation-free. Bounded by
                    // `named` (<= WORST_SILENT_NAMED) and by the same
                    // once-per-episode latch as the page above.
                    for entry in worst_silent.iter().take(named) {
                        error!(
                            code = ErrorCode::RiskGapTickGap.code_str(),
                            security_id = entry.security_id,
                            segment = entry.segment.as_str(),
                            silent_millis = entry.silent_millis,
                            expected_millis = entry.expected_millis,
                            never_ticked = entry.never_ticked,
                            "silent instrument named: quiet for {}ms against an earned cadence \
                             of {}ms{}",
                            entry.silent_millis,
                            entry.expected_millis,
                            if entry.never_ticked {
                                " — and it has produced NOTHING since being subscribed, which \
                                 usually means the subscribe did not take"
                            } else {
                                ""
                            }
                        );
                    }
                }
            }
        }
    }

    // Every sender was dropped, so no socket is left.
    //
    // ORDER MATTERS, and it is the reverse of the obvious one. Seal FIRST,
    // flush SECOND. A bucket closes only when a later tick crosses its
    // boundary, so at this instant the final bucket of every timeframe for
    // every instrument is still open and no tick will ever arrive to close
    // it. Flushing first and sealing after would push those bars into a
    // writer that has already been told the session is over.
    //
    // Skipping this step entirely is what the code did until 2026-08-11: one
    // bar per instrument per timeframe, discarded every single day, with no
    // counter moving and no log line. See `seal_open_buckets_at_close`.
    let (close_emitted, close_dropped) = ingest.seal_open_buckets_at_close();

    // Flush what is still buffered — the tail of the session is exactly the
    // data a naive shutdown loses.
    let tail = flush_and_record(&mut ingest, &feed_health);
    // Close the hand-off queue and WAIT. The tail flush above only handed the
    // batch to the writer thread; without this join the process can exit while
    // that batch is still in flight, which would lose precisely the rows the
    // tail flush exists to save. No-op on a lane that was never offloaded.
    ingest.shutdown_offload_writer();
    flush_depth(depth_ingest.as_mut());
    publish_fold_depth(&ingest);
    let depth_dropped = depth_ingest.as_ref().map_or(0, DepthIngest::dropped_rows);
    warn!(
        code = ErrorCode::WsGapConnectionState.code_str(),
        frames = seen,
        final_flush_rows = tail,
        depth_rows,
        depth_refused,
        depth_dropped,
        close_seals_emitted = close_emitted,
        close_seals_dropped = close_dropped,
        seals_emitted = ingest.seals_emitted(),
        seals_dropped = ingest.seals_dropped(),
        // Reported beside `dropped` because the pair is only readable
        // together: a non-zero `rescued` with a zero `dropped` is the no-drop
        // policy WORKING — the writer fell behind and every seal went to disk
        // — whereas the same `rescued` number alone reads like a loss. It is
        // also the capacity signal for the seal writer.
        seals_rescued = ingest.seals_rescued(),
        // Reported next to its siblings so the three are read together: a
        // large `skipped` beside a small `emitted` is the operator-timeframe
        // gate working as designed, and seeing it in isolation would invite
        // exactly the wrong conclusion. A counter with no read-out is the
        // failure mode this lane has already shipped twice.
        seals_skipped = ingest.seals_skipped(),
        seq_refused = ingest.seq_refused(),
        "Dhan live-feed frame drain ended — every socket sender was dropped, so no further \
         live ticks will be folded this session"
    );
    metrics::gauge!(FEED_STACK_UP_GAUGE).set(0.0);
    // Returned so the fold is OBSERVABLE (2026-08-11). Previously this took
    // `ingest` by value and returned `()`, which made the drain a black box:
    // its test could assert only that the future completed, and would have
    // passed just as happily on a drain that discarded every frame. The
    // production caller ignores the value; the tests are why it exists.
    DrainOutcome {
        ingest,
        frames_seen: seen,
        folded,
        depth_unconsumed,
        depth_rows,
        depth_refused,
        depth_dropped,
        unparseable,
    }
}

/// Rows buffered before a flush is forced. At ~150 B/row this is a ~150 KB ILP
/// payload — big enough to amortise the round-trip, small enough that a crash
/// loses well under a second of ticks (and the frames themselves survive in the
/// write-ahead log regardless).
pub const FLUSH_ROW_THRESHOLD: u64 = 1_000;

/// Rows buffered before a DEPTH flush is forced — 10× the tick threshold, and
/// the multiplier is the whole point.
///
/// Depth produces rows at an order of magnitude above the tick path, because
/// one packet is 20 or 200 rows rather than one. At the 250-instrument
/// depth-20 pool and one snapshot per second that is 10,000 rows/second;
/// at five snapshots per second, 50,000. Against the tick threshold of 1,000
/// that would force **10 to 50 flushes per second**, each a synchronous
/// ILP-over-HTTP round trip executed inside `block_in_place` **on the same
/// task that drains ticks**. At 5 ms per round trip the high case spends a
/// quarter of every second blocked, and the thing it blocks is the tick fold.
///
/// The tick threshold was sized by PAYLOAD (~1,000 rows ≈ 150 KB), and by that
/// measure 1,000 depth rows is also ≈160 KB — which is exactly why the reused
/// constant looked right and was not. Payload is the wrong axis here; FLUSH
/// RATE is, because the cost that matters is occupancy of the drain task.
///
/// 10,000 rows ≈ 1.6 MB per POST, and combined with the 500 ms time trigger it
/// caps depth at ~5 flushes/second in the worst modelled case and ~2 in the
/// expected one. The extra buffered rows are not a durability risk: every
/// frame behind them is already in the write-ahead log, so a crash re-folds
/// them rather than losing them.
pub const DEPTH_FLUSH_ROW_THRESHOLD: u64 = 10_000;

/// Longest a buffered row may wait before being flushed anyway, in
/// milliseconds. Half a second bounds how much of a thin instrument's tail can
/// sit unflushed without making the flush rate meaningful against the size
/// trigger.
pub const FLUSH_INTERVAL_MILLIS: u64 = 500;

/// [`FLUSH_INTERVAL_MILLIS`] as a `Duration`.
pub const FLUSH_INTERVAL: std::time::Duration =
    std::time::Duration::from_millis(FLUSH_INTERVAL_MILLIS);

/// Parses and folds ONE main-feed frame. Split out so the endpoint routing in
/// the drain reads as routing rather than as a wall of parse logic.
/// Decode one captured WebSocket frame and fold every packet it carries.
///
/// # Why this is `pub`
///
/// It is the true per-frame entry point, and the allocation this lane
/// regressed on in 2026-08-14 lived HERE — in `record_ws_lag`, called at the
/// top of the tick arm — not in `ingest_tick_at`. The DHAT gate written in
/// response measured `ingest_tick_at` alone, so the exact function it was
/// built for sat one line outside it. Exposing this closes that gap:
/// `dhat_live_ingest_seam.rs` now measures the whole frame walk.
// The guard matches tests BY NAME, and the two that drive this function are
// named for the seam they gate rather than for the callee.
// TEST-EXEMPT: driven directly by dhat_live_ingest_seam.rs — frame_drain_seam_does_not_allocate_per_tick + frame_drain_gate_is_not_vacuous
pub fn drain_main_feed_frame(
    ingest: &mut LiveIngest,
    frame: &CapturedFrame,
    received_at_nanos: i64,
    recv_millis: u64,
    c: &DrainCounters,
) -> FrameOutcome {
    let mut out = FrameOutcome::default();
    // A single WebSocket message may carry SEVERAL stacked packets — the
    // frame cap is `MAX_PACKETS_PER_FRAME` (70,000) of them. Walking the frame
    // packet by packet is what stops packets 2..N being silently discarded.
    let mut offset = 0usize;
    let mut packets = 0u32;
    while offset < frame.bytes.len() {
        let Some(len) = main_feed_packet_len(&frame.bytes[offset..]) else {
            // Unrecognised code or a trailing partial packet: stop here rather
            // than resynchronising on a guess, which would fabricate ticks.
            c.unparseable.increment(1);
            out.unparseable = out.unparseable.saturating_add(1);
            // The outcome counter above says "a frame gave up". THIS says how
            // much of it was thrown away, which is the number that decides
            // whether this was one stray packet or most of a 1,600-packet
            // message.
            let abandoned = frame.bytes.len().saturating_sub(offset) as u64;
            c.abandoned_bytes.increment(abandoned);
            out.abandoned_bytes = out.abandoned_bytes.saturating_add(abandoned);
            return out;
        };
        let end = offset.saturating_add(len);
        if end > frame.bytes.len() {
            c.truncated.increment(1);
            let abandoned = frame.bytes.len().saturating_sub(offset) as u64;
            c.abandoned_bytes.increment(abandoned);
            out.abandoned_bytes = out.abandoned_bytes.saturating_add(abandoned);
            return out;
        }
        match dispatch_frame(&frame.bytes[offset..end], received_at_nanos) {
            Ok(parsed @ (ParsedFrame::Tick(_) | ParsedFrame::TickWithDepth(..))) => {
                // Full mode carries 5 levels of bid/ask in EVERY tick packet.
                // Until 2026-08-19 this arm bound them to `_` and threw them
                // away — the lane paid 3.24x the bandwidth of Quote mode for a
                // book it discarded, while separately storing 20 levels for a
                // 250-instrument subset at ~506M rows/day.
                //
                // Not free: at the 25,000 target it is ~611M rows/day of its
                // own. The cheaper shape would have been a SWAP — standing the
                // dedicated depth-20 pool down and trading 20 levels on 250
                // instruments for 5 levels on all 25,000, which is less
                // storage AND 100x the coverage. The operator was offered that
                // swap on 2026-08-19 and chose to keep all THREE sources, so
                // this runs alongside them rather than instead of them.
                //
                // CORRECTED 2026-08-20: this closed with "so this ships able
                // but off", which stopped being true the same day it was
                // written — the boot site wires the sink unconditionally.
                //
                // The ingest-shed gate is consulted FIRST and cheaply: one
                // relaxed atomic load, before the sink is even taken. Inline
                // depth is the first thing shed on a filling disk because it
                // is the widest and most redundant — 10 rows per packet across
                // the whole universe, on instruments the dedicated feeds
                // already cover more deeply. The tick itself is NEVER shed.
                if let (Some(sink), ParsedFrame::TickWithDepth(t, levels)) =
                    (ingest.inline_depth.as_mut(), &parsed)
                {
                    if INGEST_SHED.allows_inline_depth() {
                        // `frame.seq` and `packets` are BOTH load-bearing — see
                        // the `capture_seq` derivation inside. The frame stamp
                        // alone repeats across every packet in the frame, and
                        // the level ordinal alone repeats across every frame.
                        out.inline_depth_rows =
                            out.inline_depth_rows.saturating_add(append_inline_depth(
                                sink,
                                t,
                                levels,
                                received_at_nanos,
                                frame.seq,
                                packets,
                                c,
                            ));
                    } else {
                        c.shed_inline_depth.increment(1);
                    }
                }
                let tick = match parsed {
                    ParsedFrame::Tick(t) | ParsedFrame::TickWithDepth(t, _) => t,
                    // Unreachable: the match arm above admits only these two.
                    _ => unreachable!("arm admits only Tick and TickWithDepth"),
                };
                // Delivery lag, per SOCKET. Recorded here because this is the
                // only point where the exchange stamp, the receipt instant and
                // the originating connection are all in hand.
                //
                // Only packet types that actually carry an LTT reach this arm —
                // OI, PrevClose and MarketStatus decode to non-`Tick` variants
                // and never appear here — so a missing timestamp is a garbage
                // timestamp, not an absent one, and is EXCLUDED rather than
                // recorded as zero.
                record_ws_lag(frame.connection_index, &tick, received_at_nanos);
                // `frame.seq` is per-FRAME, but `capture_seq` must be unique
                // per ROW or two ticks in one message would collapse into one
                // under the DEDUP key. The packet index is folded in.
                match ingest.ingest_tick_at(&tick, frame.seq, packets, recv_millis) {
                    IngestOutcome::Folded { .. } => {
                        c.folded.increment(1);
                        out.folded = out.folded.saturating_add(1);
                    }
                    IngestOutcome::WrittenOutOfSession => {
                        // A ROW was written, so it counts as folded for the
                        // purpose of "did this frame produce data?" — the
                        // per-tick `out_of_session` counter already records
                        // that no candle opened, and double-counting the same
                        // tick as a failure here would make the drain's frame
                        // mix read as though the pre-open were broken.
                        c.folded.increment(1);
                        out.folded = out.folded.saturating_add(1);
                    }
                    IngestOutcome::SeqUnrepresentable => c.seq_unrepresentable.increment(1),
                    IngestOutcome::AggregatorRefused => c.aggregator_refused.increment(1),
                    IngestOutcome::WriteFailed => c.write_failed.increment(1),
                }
            }
            // A disconnect the drain DECODED. It is split out of the untyped
            // `non_tick` bucket above because of a divergence found on
            // 2026-08-26: `classify_frame` requires the WHOLE frame to walk
            // cleanly before it will act on a stacked disconnect (deliberate --
            // it stops a 16-byte frame whose first byte happens to be 50 from
            // parking a healthy socket on a reason read out of random data),
            // while THIS walk stops AT the bad packet, having already decoded
            // everything before it. So `[disconnect 804][unknown code]`
            // reaches here as a real, parsed 804 that the classifier called
            // `Data`.
            //
            // That asymmetry is not resolved here -- relaxing the classifier
            // would trade away a documented fail-safe -- but the reason no
            // longer vanishes. 804 means the subscribe set exceeds the
            // per-connection cap and is ruled Fatal precisely because retrying
            // re-sends the identical over-limit set forever and can earn an
            // 805 account block, so "the reason reached no log and no metric"
            // was the part that made it unfixable from outside.
            //
            // Not throttled: a disconnect packet ends the socket, so this is
            // bounded by reconnects, not by frame rate.
            Ok(ParsedFrame::Disconnect(reason)) => {
                c.main_feed_disconnects.increment(1);
                out.disconnects = out.disconnects.saturating_add(1);
                error!(
                    code = ErrorCode::WsGapConnectionState.code_str(),
                    source = "drain_decoded_disconnect",
                    reason = ?reason,
                    "the main feed decoded a DISCONNECT packet inside a frame — \
                     if this reason is not also on a socket-close line, the \
                     classifier could not act on it and the reconnect ladder is \
                     treating a possibly-fatal reason as transient"
                );
            }
            // Non-tick frames are real protocol traffic, not errors: OI and
            // previous-close arrive as their own packets, market-status and
            // disconnect are control. Counted so the traffic mix is visible,
            // deliberately not folded — none of them carries an LTP.
            Ok(_) => c.non_tick.increment(1),
            Err(_) => {
                // The dispatcher already counts unknown response codes and
                // logs protocol drift; a second log line here would amplify a
                // malformed-frame storm into a log flood.
                c.unparseable.increment(1);
                out.unparseable = out.unparseable.saturating_add(1);
            }
        }
        offset = end;
        packets = packets.saturating_add(1);
        if packets >= MAX_PACKETS_PER_FRAME {
            // A frame claiming more packets than the protocol can produce is
            // malformed or hostile; stop walking rather than loop on it.
            c.truncated.increment(1);
            return out;
        }
    }
    out
}

/// What one main-feed frame produced.
///
/// These numbers are already published as metrics; this struct exists so they
/// are also RETURNABLE. Metrics are process-global and awkward to assert on,
/// which is precisely why the drain's tests could only check that it finished.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct FrameOutcome {
    /// Packets in this frame that became a buffered row.
    pub folded: u64,
    /// DISCONNECT packets this frame carried and the drain decoded. Split
    /// out of the untyped non-tick count on 2026-08-26 so a reason the
    /// classifier could not act on is still visible — see the drain arm.
    pub disconnects: u64,
    /// Packets refused by the parser or by an unknown response code.
    pub unparseable: u64,
    /// Depth rows appended from the 5 levels carried INLINE in Full-mode tick
    /// packets. Zero unless the ingest was built with
    /// [`LiveIngest::with_inline_depth`] — which the production boot site does
    /// unconditionally. (CORRECTED 2026-08-20: said "unless `[dhan_feed]
    /// persist_full_mode_depth` is on", a key that exists nowhere.)
    pub inline_depth_rows: u64,
    /// Bytes of this frame the walk never decoded, because it hit an unknown
    /// response code or a trailing partial packet and stopped rather than
    /// guessing. Returned as well as counted so a test can assert the
    /// magnitude — the whole reason this struct exists.
    pub abandoned_bytes: u64,
}

/// What one depth frame produced.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct DepthFrameOutcome {
    /// Level rows appended to the ILP buffer.
    pub rows: u64,
    /// Packets the parser refused, or whose segment code could not be mapped.
    ///
    /// A refused packet is a packet whose levels are NOT in the table. It is
    /// counted separately from `rows` precisely so "we captured everything"
    /// can be checked rather than assumed.
    pub refused: u64,
    /// Server-initiated disconnect packets seen on a depth socket.
    pub disconnects: u64,
}

/// Depth capture state — the writer plus the reusable level buffer.
///
/// One per drain task, mirroring `LiveIngest`. The [`DepthLevelBuffer`] is
/// allocated ONCE here and passed `&mut` to every parse, which is what keeps
/// the depth-200 path from allocating 3.2 KB of levels per packet on a stream
/// that can deliver several packets per frame.
pub struct DepthIngest {
    writer: DepthWriter,
    buf: DepthLevelBuffer,
}

impl DepthIngest {
    /// Production constructor.
    #[must_use]
    // TEST-EXEMPT: thin constructor; every behaviour is exercised through
    // `drain_depth_frame` in the tests below via `for_test`.
    pub fn new(questdb: &tickvault_common::config::QuestDbConfig) -> Self {
        Self {
            writer: DepthWriter::new(questdb, Feed::Dhan),
            buf: DepthLevelBuffer::new(),
        }
    }

    /// Test constructor — disconnected writer, same buffer.
    #[must_use]
    // TEST-EXEMPT: test-only helper used by the depth drain tests below.
    pub fn for_test() -> Self {
        Self {
            writer: DepthWriter::for_test(Feed::Dhan),
            buf: DepthLevelBuffer::new(),
        }
    }

    /// Rows appended but not yet flushed.
    #[must_use]
    // TEST-EXEMPT: observability accessor, asserted by the depth drain tests.
    pub fn pending_rows(&self) -> usize {
        self.writer.pending()
    }

    /// The ILP text this ingest has buffered — what a test must read to check
    /// that the `side` / `depth_kind` / `capture_seq` MAPPING done in
    /// [`drain_depth_frame`] is right. Row counts cannot see that mapping.
    #[must_use]
    // Asserted by the drain-label and capture_seq tests, which can only exist
    // because this does.
    // TEST-EXEMPT: observability accessor, asserted by the tests it enables.
    pub fn pending_ilp(&self) -> String {
        self.writer.buffer_utf8()
    }

    /// Rows discarded on failed flushes.
    #[must_use]
    // TEST-EXEMPT: observability accessor, asserted by the depth drain tests.
    pub fn dropped_rows(&self) -> u64 {
        self.writer.dropped()
    }

    /// Flushes the depth ILP buffer.
    ///
    /// # Errors
    /// Propagates the writer's flush failure (which has already discarded and
    /// logged the pending rows).
    pub fn flush(&mut self) -> anyhow::Result<()> {
        self.writer.flush()
    }
}

/// Folds ONE depth frame into level rows.
///
/// A depth frame stacks packets exactly as a main-feed frame does — typically
/// `[Inst1 Bid][Inst1 Ask][Inst2 Bid]…` — so it is walked packet by packet.
/// Each side-packet becomes one row PER LEVEL: 20 rows for depth-20, up to 200
/// for depth-200. That row count is the operator's explicit instruction
/// (2026-08-15: every level of both pools, nothing sampled), and it is also
/// why this function never allocates: at 200 rows per packet a per-packet
/// allocation would be the single hottest allocation site in the process.
///
/// Refusals are COUNTED, never silent. A packet whose segment byte maps to no
/// known segment is refused rather than written under a guessed identity: a
/// wrong segment writes the row against the wrong instrument (I-P1-11), which
/// is worse than not writing it, and worse still because it looks like data.
/// Appends the 5 depth levels carried inline in a Full-mode tick packet.
///
/// Returns the number of rows appended (10 per packet: 5 bid + 5 ask), so the
/// drain can count what this path actually produced rather than inferring it.
///
/// # Why this is safe on the per-tick path
///
/// Ten `DepthRow` values built on the STACK and handed straight to the ILP
/// writer. No `Vec`, no `String`, no per-row allocation — `segment` and the
/// `d5`/`bid`/`ask` labels are all `&'static str`. Cost is the ILP append
/// itself, which the dedicated depth pools already pay per level.
///
/// # Zero prices are WRITTEN, matching the dedicated drain
///
/// My first version skipped levels priced at zero, reasoning that a zero looks
/// like a real bid at zero rupees. The dedicated depth drain does the opposite
/// and documents why: depth is a FIXED level count, so an illiquid contract
/// with three real bids still emits every level and the rest are legitimately
/// all-zero — the documented absent-level sentinel. Refusing them would count
/// normal book shape as corruption AND delete the operator's own
/// show-me-everything view of how deep a book actually is.
///
/// Had `d5` skipped them, the same instrument would report 5 levels in one
/// table and 3 in another for the same instant, and the difference would look
/// like data loss rather than a convention mismatch. Negative and implausible
/// prices ARE refused, exactly as they are there.
///
/// # `capture_seq` is PER-PACKET, exactly as the dedicated drain derives it
///
/// A Dhan frame STACKS packets, `received_at_nanos` is computed ONCE for the
/// whole frame, and `capture_seq` is a DEDUP key column. Until 2026-08-20 this
/// stamped `level_no * 2 (+1)` — a value that depends only on the level and the
/// side, so two Full packets for the SAME instrument in ONE frame produced ten
/// rows whose EVERY key column matched ten earlier rows, and QuestDB upserted
/// one book silently over the other. `rows` counts successful ILP APPENDS, not
/// DB acceptance, so the loss did not even show as a shortfall in our own
/// counter — invisible loss in the one table whose entire premise is that
/// nothing is lost. That is the identical defect the dedicated depth drain
/// fixed on 2026-08-15; this path was written four days later and repeated it.
///
/// The per-packet value is used for all ten rows because `side` and `level` are
/// THEMSELVES key columns — they already separate the rows within one packet.
/// What was missing was the packet's identity, not the level's.
///
/// An index that does not fit the bits `packet_capture_seq` reserves REFUSES
/// the packet's depth and counts it. Never a fallback value: pinning every
/// over-range packet onto one key would collapse them together, which is the
/// same silent merge this exists to remove, reintroduced at the other end.
fn append_inline_depth(
    sink: &mut DepthIngest,
    tick: &tickvault_common::tick_types::ParsedTick,
    levels: &[tickvault_common::tick_types::MarketDepthLevel; 5],
    received_at_nanos: i64,
    frame_seq: u64,
    packet_index: u32,
    c: &DrainCounters,
) -> u64 {
    // Same posture as the dedicated depth drain: an unrecognised segment is
    // REFUSED, never written under a placeholder. A row labelled "UNKNOWN"
    // would silently merge distinct instruments under one segment value.
    let Some(segment) = depth_segment_label(tick.exchange_segment_code) else {
        // 2026-08-25: was a SILENT return. `DEPTH_COUNTER`'s own doc already
        // promised `refused` covered "an unmappable segment code", and the
        // dedicated depth drain honours that; this inline twin, written four
        // days later, dropped ten rows per packet with no counter and no log.
        // A reader auditing the counter would have concluded d5 losses were
        // visible when they were not.
        c.depth_refused.increment(1);
        return 0;
    };
    // A value above i64::MAX cannot be a real Dhan id. Refuse rather than
    // saturate: saturating writes every such packet under one bogus id.
    let Ok(security_id) = i64::try_from(tick.security_id) else {
        // 2026-08-25: was a SILENT return, same class as the segment arm above.
        c.depth_refused.increment(1);
        return 0;
    };
    let Some(capture_seq) =
        tickvault_storage::ws_frame_spill::packet_capture_seq(frame_seq, u64::from(packet_index))
            .and_then(capture_seq_from_frame_seq)
    else {
        c.depth_refused.increment(1);
        return 0;
    };
    // IST, not UTC — `received_at_nanos` is deliberately TRUE UTC because
    // `ws_lag_ms` differences the vendor's IST stamp against it, so the offset
    // is added at each PERSISTING site instead. The dedicated depth drain does
    // exactly this; this path shipped without it on 2026-08-19, which put `d5`
    // rows 5h30m behind `d20`/`d200` rows in the SAME table (any join or
    // eyeball comparison silently misaligns) and, because `ts` is the
    // DESIGNATED timestamp, partitioned every row stamped between 18:30 and
    // 23:59 IST into the PREVIOUS day — the day archival and retention key on.
    let ts_nanos =
        received_at_nanos.saturating_add(tickvault_common::constants::IST_UTC_OFFSET_NANOS);
    let mut rows = 0_u64;
    for (idx, level) in levels.iter().enumerate() {
        let level_no = i64::try_from(idx).unwrap_or(i64::MAX).saturating_add(1);
        // PRICE PRECISION — `f32_to_f64_clean`, never bare `f64::from`.
        //
        // 2026-08-20. These are the ONLY f32 prices in this file: the inline
        // 5-level book rides inside the 162-byte Full packet, where Dhan
        // sends prices as f32, while the dedicated depth-20/200 sockets send
        // native f64. So the two writers a few hundred lines apart look
        // superficially identical and are not — and the d5 half shipped with
        // `f64::from`, widening the IEEE-754 bit pattern: 10.20 became
        // 10.19999980926514, 23925.65 became 23925.650390625.
        //
        // Both land in the SAME `market_depth` table, so one instrument at one
        // instant carried exact decimals on its d20 rows and 12-digit
        // artifacts on its d5 rows, and any price equality against `ticks`
        // (which has always applied this conversion) silently missed. Option
        // premiums sit on x.05/x.20/x.35 constantly, so this bit almost every
        // level rather than rarely.
        //
        // Rounding is deliberately NOT applied on top: after this conversion
        // the value is already the shortest decimal that round-trips through
        // f32, which for any real tick-size price IS the operator-visible
        // number — and the d20/d200 rows in the same table are unrounded, so
        // adding it here would trade one inconsistency for another.
        let plausible = |p: f32| -> bool {
            let p = f64::from(p);
            p.is_finite() && p >= 0.0 && p <= f64::from(MAX_PLAUSIBLE_LTP)
        };
        if plausible(level.bid_price) {
            let row = DepthRow {
                security_id,
                segment,
                depth_kind: DEPTH_KIND_5,
                side: DEPTH_SIDE_BID,
                level: level_no,
                price: tickvault_common::price_precision::f32_to_f64_clean(level.bid_price),
                quantity: i64::from(level.bid_quantity),
                orders: i64::from(level.bid_orders),
                capture_seq,
                ts_nanos,
            };
            if sink.writer.append_row(&row).is_ok() {
                rows = rows.saturating_add(1);
            } else {
                // 2026-08-25: the dedicated drain has this else arm; the
                // inline twin did not, so an ILP append failure lost the row
                // in silence.
                c.depth_refused.increment(1);
            }
        } else {
            c.depth_refused.increment(1);
        }
        if plausible(level.ask_price) {
            let row = DepthRow {
                security_id,
                segment,
                depth_kind: DEPTH_KIND_5,
                side: DEPTH_SIDE_ASK,
                level: level_no,
                price: tickvault_common::price_precision::f32_to_f64_clean(level.ask_price),
                quantity: i64::from(level.ask_quantity),
                orders: i64::from(level.ask_orders),
                capture_seq,
                ts_nanos,
            };
            if sink.writer.append_row(&row).is_ok() {
                rows = rows.saturating_add(1);
            } else {
                c.depth_refused.increment(1);
            }
        } else {
            c.depth_refused.increment(1);
        }
    }
    rows
}

fn drain_depth_frame(
    depth: &mut DepthIngest,
    frame: &CapturedFrame,
    received_at_nanos: i64,
    kind: DepthFeedKind,
    c: &DrainCounters,
) -> DepthFrameOutcome {
    let mut out = DepthFrameOutcome::default();
    let depth_kind_label = match kind {
        DepthFeedKind::Twenty => DEPTH_KIND_20,
        DepthFeedKind::TwoHundred => DEPTH_KIND_200,
    };
    let mut iter = split_depth_frame(&frame.bytes, kind);
    // `by_ref` rather than consuming the iterator: `stop_reason()` and
    // `length_field_mismatches()` are read AFTER the walk, and they are the
    // only way to tell a cleanly-consumed frame from a truncated one.
    //
    // `enumerate` is load-bearing, not cosmetic — see the `capture_seq`
    // derivation inside the loop.
    for (packet_index, packet_bytes) in iter.by_ref().enumerate() {
        // PER-PACKET `capture_seq`, exactly as `ingest_tick_at` derives it.
        //
        // A Dhan frame STACKS packets, and `capture_seq` is a DEDUP key column.
        // Stamping the bare `frame.seq` on every packet in the frame — which
        // this did until 2026-08-15 — makes all eight key columns identical for
        // any two packets sharing `(security_id, segment, side)` in ONE frame,
        // so QuestDB upserts one silently away. Worse, `out.rows` counts
        // successful ILP APPENDS, not DB acceptance, so the loss would not even
        // show up as a shortfall in our own counter — invisible loss in the one
        // table whose entire premise is that nothing is lost.
        //
        // `next_frame_seq` deliberately zeroes the low `MAX_PACKET_INDEX` bits
        // to reserve exactly this packet slot; the bare-seq version left all
        // 131,071 of them unused. Both narrowings REFUSE rather than saturate:
        // an `unwrap_or(i64::MAX)` would pin every over-range packet onto one
        // key and collapse them together — the same silent-merge this fix
        // exists to remove, reintroduced at the other end.
        let Some(packet_seq) =
            tickvault_storage::ws_frame_spill::packet_capture_seq(frame.seq, packet_index as u64)
        else {
            out.refused = out.refused.saturating_add(1);
            c.depth_refused.increment(1);
            continue;
        };
        let Some(capture_seq) = capture_seq_from_frame_seq(packet_seq) else {
            out.refused = out.refused.saturating_add(1);
            c.depth_refused.increment(1);
            continue;
        };
        let parsed = match parse_depth_packet(packet_bytes, kind, &mut depth.buf) {
            Ok(p) => p,
            Err(_) => {
                out.refused = out.refused.saturating_add(1);
                c.depth_refused.increment(1);
                continue;
            }
        };
        let header = parsed.header;
        let (side_label, levels) = match parsed.payload {
            DepthPayload::Levels { side, levels } => {
                let label = match side {
                    DepthSide::Bid => DEPTH_SIDE_BID,
                    DepthSide::Ask => DEPTH_SIDE_ASK,
                };
                (label, levels)
            }
            DepthPayload::Disconnect { .. } => {
                out.disconnects = out.disconnects.saturating_add(1);
                c.depth_disconnects.increment(1);
                continue;
            }
        };
        let Some(segment) = depth_segment_label(header.exchange_segment_code) else {
            out.refused = out.refused.saturating_add(1);
            c.depth_refused.increment(1);
            continue;
        };
        // `security_id` is `u64` on the parsed header (widened from the wire's
        // u32). A value above `i64::MAX` cannot be a real Dhan id, so it is
        // REFUSED rather than saturated: saturating would write every such
        // packet under one bogus id, silently merging distinct instruments.
        let Ok(security_id) = i64::try_from(header.security_id) else {
            out.refused = out.refused.saturating_add(1);
            c.depth_refused.increment(1);
            continue;
        };
        for (idx, level) in levels.iter().enumerate() {
            // Price sanity, per level — the depth twin of `tick_price_is_sane`.
            //
            // `level.price` is `read_f64_le` straight off the wire, so EVERY
            // 8-byte pattern is a valid `f64`: NaN, ±Inf, negative, 1e308. Two
            // things go wrong without this gate, and the second is the worse
            // one:
            //
            //   1. A NaN or absurd price renders through `market_depth_named`
            //      as a real book price.
            //   2. If the server REJECTS the resulting line, `flush` fails and
            //      `discard_pending` clears the ENTIRE pending buffer — up to
            //      `DEPTH_FLUSH_ROW_THRESHOLD` rows of perfectly good levels
            //      from every other instrument in the batch. One poisoned
            //      level from one instrument would cost everyone else's book.
            //      Refusing per ROW turns a batch-wide loss into a single
            //      counted row.
            //
            // ZERO IS ACCEPTED and is not corruption: depth-20 is a FIXED 20
            // levels (`depth_level_count`), so an illiquid contract with three
            // real bids still emits 20 rows and levels 4..20 are legitimately
            // all-zero — the documented absent-level sentinel. Refusing them
            // would count normal book shape as corruption and, worse, delete
            // the operator's own "show me everything" view of how deep a book
            // actually is. Negative is refused; a price cannot be below zero.
            if !level.price.is_finite()
                || level.price < 0.0
                || level.price > f64::from(MAX_PLAUSIBLE_LTP)
            {
                out.refused = out.refused.saturating_add(1);
                c.depth_refused.increment(1);
                continue;
            }
            let row = DepthRow {
                security_id,
                segment,
                depth_kind: depth_kind_label,
                side: side_label,
                // 1-based: level 1 is the best price. `idx` is bounded by 200,
                // so the +1 cannot overflow an i64.
                level: i64::try_from(idx).unwrap_or(i64::MAX).saturating_add(1),
                price: level.price,
                quantity: i64::from(level.quantity),
                orders: i64::from(level.orders),
                capture_seq,
                // 2026-08-19 — IST, not UTC. Operator: "why the market depth ts
                // has utc time it should be the precise ist".
                //
                // He is right, and depth was the ONLY table getting this
                // wrong. `received_at_nanos` is deliberately TRUE UTC —
                // `ws_lag_ms` converts the vendor's IST exchange stamp back to
                // UTC to difference against it, so that value must NOT be
                // shifted at its source. Every table that PERSISTS a
                // wall-clock instant adds the offset at its own stamping site
                // (`tick_persistence` does exactly this for
                // `received_at_ist_nanos`, as does `partition_archive`).
                // Depth skipped that step and wrote raw UTC into a column
                // every sibling table stores as IST.
                //
                // Two real consequences, not cosmetics: depth rows read 5h30m
                // behind every other table in the console, so any join or
                // eyeball comparison against ticks silently misaligns; and
                // because `ts` is the DESIGNATED timestamp, rows between 18:30
                // and 23:59 IST were partitioned into the PREVIOUS day —
                // which is also the day the archival and retention paths key
                // on.
                ts_nanos: received_at_nanos
                    .saturating_add(tickvault_common::constants::IST_UTC_OFFSET_NANOS),
            };
            if depth.writer.append_row(&row).is_ok() {
                out.rows = out.rows.saturating_add(1);
            } else {
                out.refused = out.refused.saturating_add(1);
                c.depth_refused.increment(1);
            }
        }
    }

    // A frame that did not consume cleanly is a frame whose tail we could not
    // read. Counted, never assumed empty: silently treating a truncated frame
    // as "no more packets" is how a partial book reads as a complete one.
    if iter.stop_reason() != Some(DepthSplitStop::Complete) {
        out.refused = out.refused.saturating_add(1);
        c.depth_refused.increment(1);
    }
    if iter.length_field_mismatches() > 0 {
        c.depth_length_mismatch
            .increment(u64::from(iter.length_field_mismatches()));
    }
    c.depth_rows.increment(out.rows);
    out
}

/// What a whole drain run produced, plus the ingest it produced it with.
///
/// Returned rather than discarded so the socket→store seam is observable from
/// a test. Without it, `run_frame_drain` is a black box whose only assertable
/// property is that it terminates — and a drain that silently threw every
/// frame away terminates just as promptly as one that works.
pub struct DrainOutcome {
    /// The ingest, after the final flush.
    pub ingest: LiveIngest,
    /// Frames taken off the ring.
    pub frames_seen: u64,
    /// Packets folded into buffered rows.
    pub folded: u64,
    /// Depth frames that reached the drain with NO depth ingest wired.
    ///
    /// Before 2026-08-15 this counted EVERY depth frame, because there was no
    /// consumer by design. It now counts only the wiring bug, and should be
    /// zero whenever a depth socket is open.
    pub depth_unconsumed: u64,
    /// Depth LEVEL rows appended to `market_depth`.
    pub depth_rows: u64,
    /// Depth packets refused — parse error, unmappable segment, truncated
    /// tail, or an ILP append failure. Levels that arrived and are NOT stored.
    pub depth_refused: u64,
    /// Depth rows discarded by a failed flush. Distinct from `depth_refused`:
    /// these were validly parsed and buffered, then lost at the database.
    pub depth_dropped: u64,
    /// Packets the parser refused.
    pub unparseable: u64,
}

/// Re-exported so the drain and the WS classifier walk the SAME bound. The
/// declaration moved to `common` on 2026-08-25 (see there for the arithmetic
/// correction and why the ceiling is a policy bound, not a capacity one).
pub use tickvault_common::constants::MAX_PACKETS_PER_FRAME;

/// Prometheus histogram of exchange→receipt delivery lag on the LIVE socket.
///
/// The `_ms` suffix is load-bearing, not cosmetic: `observability.rs` matches
/// `Matcher::Suffix("_ms")` to install the millisecond bucket set. A `_seconds`
/// name renders as a summary with no `_bucket` series, and the panel would have
/// nothing to read.
pub const WS_LAG_HISTOGRAM: &str = "tv_dhan_ws_lag_ms";

/// Ticks deliberately EXCLUDED from the lag histogram, by reason.
///
/// Counted rather than recorded-as-zero. A packet with no usable exchange
/// timestamp is not a zero-latency tick, and folding it in as `0.0` would drag
/// every percentile toward zero and make a degrading feed look like it was
/// getting faster.
pub const WS_LAG_EXCLUDED_COUNTER: &str = "tv_dhan_ws_lag_excluded_total";

/// Delivery lag in milliseconds, or `None` when this tick must be EXCLUDED.
///
/// # The offset rule this encodes
///
/// `ParsedTick::exchange_timestamp` is **IST epoch seconds** — IST wall-clock
/// rendered as an epoch — while `received_at_nanos` is true UTC epoch nanos.
/// To compare them the exchange stamp must have the IST offset **SUBTRACTED**:
///
/// ```text
/// lag_ms = received_ms − (ltt_secs − 19_800) × 1000
/// ```
///
/// Adding the offset is the single most destructive mistake available here —
/// `data-integrity.md` calls the WebSocket timestamp rule "THE SINGLE MOST
/// CRITICAL DATA INTEGRITY RULE" — and it would not look broken: it would
/// report a steady 39,600,000 ms (11 h) lag, which a reader could mistake for a
/// unit bug rather than a sign error. A test pins the exact zero case.
///
/// # Why `None` rather than a number
///
/// - `ltt < MIN_PLAUSIBLE_EXCHANGE_TS_SECS` — a zero or garbage stamp would
///   render as a ~55-year lag and destroy every percentile in the bucket set.
/// - Packets carrying no LTT at all (OI code 5, PrevClose 6, MarketStatus 7)
///   never reach this function; only ticker (2), quote (4) and full (8) parsers
///   populate `exchange_timestamp`.
///
/// # The ±1 s floor, stated where it is computed
///
/// Dhan sends LTT as whole SECONDS. Truncation alone therefore makes a tick
/// look up to ~1 s early, so a genuinely-fast delivery can compute NEGATIVE.
/// Negatives clamp to zero and are counted separately — never recorded as a
/// negative, and never claimed as sub-second precision. This measures outages
/// and drift honestly; it cannot measure microseconds, and no arithmetic here
/// can recover precision the vendor never transmitted.
#[must_use]
pub fn ws_lag_ms(exchange_timestamp: u32, received_at_nanos: i64) -> Option<WsLag> {
    if exchange_timestamp
        < tickvault_trading::candles::multi_tf_aggregator::MIN_PLAUSIBLE_EXCHANGE_TS_SECS
    {
        return None;
    }
    let ltt_utc_secs = i64::from(exchange_timestamp)
        - i64::from(tickvault_common::constants::IST_UTC_OFFSET_SECONDS);
    let received_ms = received_at_nanos / 1_000_000;
    let lag_ms = received_ms - ltt_utc_secs.saturating_mul(1_000);
    if lag_ms < 0 {
        return Some(WsLag::ClampedNegative);
    }
    // i64 -> f64 loses precision only above 2^53, which is ~285,000 YEARS
    // expressed in milliseconds. Every value reaching here is a delivery lag,
    // bounded in practice by the reconnect ladder and in principle by the
    // plausibility floor above, so the lossy range is unreachable. The metrics
    // crate takes f64, so the conversion is required, not incidental.
    // APPROVED: lossy range (>2^53 ms ~ 285,000 years) is unreachable for a lag.
    #[allow(clippy::cast_precision_loss)]
    Some(WsLag::Measured(lag_ms as f64))
}

/// Record one tick's delivery lag against the socket it arrived on.
///
/// Labelled by `connection`, not by instrument. That is a cost decision with a
/// hard number behind it: at the ~4,565-instrument live universe a per-instrument
/// label would be ~4,565 CloudWatch series ≈ $1,369/mo, against a budget whose
/// automatic action is `STOP_EC2_INSTANCES` — the observability feature would
/// stop the trading box. Sixteen connection slots is ≈$4.80/mo.
///
/// It is also the more useful cut. Per-instrument lag is dominated by how often
/// that instrument TRADES (LTT is last-trade time, so a thin option is
/// legitimately minutes stale and would page constantly); per-socket lag
/// isolates the thing we can act on — one connection delivering late.
/// Pre-resolved lag-metric handles, one histogram per connection slot.
///
/// # Why this exists
///
/// `record_ws_lag` runs on the PER-TICK path. The previous form built its label
/// with `connection_index.to_string()` and passed it to `metrics::histogram!`,
/// which constructs a `Key` owning a `Vec<Label>` — so recording a tick's own
/// latency cost **two heap allocations per tick**, on the one path this module
/// promises is allocation-free.
///
/// That is the identical defect `parser/dispatcher.rs:32-35` and `DrainCounters`
/// (above, in this file) already solved. The label set here is bounded and known
/// at compile time — `MAX_TOTAL_DHAN_CONNECTIONS` slots — so every handle is
/// resolved ONCE and recording becomes a plain atomic update.
///
/// The emitted series are byte-identical to before
/// (`tv_dhan_ws_lag_ms{connection="0".."15"}`), which is what makes this a safe
/// refactor: no dashboard, alarm, or EMF selector can tell the difference.
struct WsLagHandles {
    /// Indexed by connection slot. Built once; never resized.
    per_connection: [metrics::Histogram; MAX_TOTAL_DHAN_CONNECTIONS as usize],
    /// Fallback for a slot outside the pool budget. Should be unreachable —
    /// `ConnectionSlot` is allocated from the same budget — but a hot-path
    /// index must never panic and must never allocate, so it degrades into a
    /// counted bucket instead.
    unknown_connection: metrics::Histogram,
    unknown_slot: metrics::Counter,
    excluded_clamped_negative: metrics::Counter,
    excluded_implausible_ltt: metrics::Counter,
}

impl WsLagHandles {
    fn new() -> Self {
        Self {
            // `to_string()` here runs at most 16 times, at first-tick, on the
            // cold path — not per tick. That is the whole point of the cache.
            per_connection: std::array::from_fn(
                |slot| metrics::histogram!(WS_LAG_HISTOGRAM, "connection" => slot.to_string()),
            ),
            unknown_connection: metrics::histogram!(
                WS_LAG_HISTOGRAM,
                "connection" => "unknown"
            ),
            unknown_slot: metrics::counter!(
                WS_LAG_EXCLUDED_COUNTER,
                "reason" => "unknown_connection_slot"
            ),
            excluded_clamped_negative: metrics::counter!(
                WS_LAG_EXCLUDED_COUNTER,
                "reason" => "clamped_negative"
            ),
            excluded_implausible_ltt: metrics::counter!(
                WS_LAG_EXCLUDED_COUNTER,
                "reason" => "implausible_ltt"
            ),
        }
    }

    /// The histogram for one slot. Out-of-range degrades to a counted bucket —
    /// never a panic, never an allocation.
    fn histogram_for(&self, connection_index: u8) -> &metrics::Histogram {
        match self.per_connection.get(connection_index as usize) {
            Some(histogram) => histogram,
            None => {
                self.unknown_slot.increment(1);
                &self.unknown_connection
            }
        }
    }
}

/// Per-connection last-tick instant, in epoch MILLISECONDS. `0` = never ticked.
///
/// # The failure this exists for
///
/// A socket that keeps answering pings but stops delivering data never trips
/// the idle watchdog — the watchdog governs SILENCE on the wire, and a socket
/// sending pongs is not silent. It also never reconnects, so the whole
/// reconnect family (`tv_dhan_ws_reconnect_total`, `_dial_failed_total`,
/// `_subscribe_failed_total`) stays flat: alarming those, the obvious fix,
/// cannot catch this case BY CONSTRUCTION, because the defining property of a
/// deaf socket is that nothing about it is retrying.
///
/// The lane-level `LAST_TICK_AGE_GAUGE` cannot see it either. With fifteen
/// sockets delivering normally the lane's last tick is always a second old,
/// however dead the sixteenth is.
///
/// # Why ONE gauge and not sixteen
///
/// Per-connection series would be sixteen names' worth of cost (~$4.80/mo by
/// the 2026-08-14 noise-lock figure) to answer a yes/no question. Publishing
/// the WORST age across connections answers it for one series (~$0.30/mo) with
/// identical detection power: if socket 7 goes deaf, the max climbs while the
/// lane gauge stays at one second. Per-connection attribution is still on
/// `/metrics` for whoever is triaging, which is the moment attribution is
/// actually wanted — and by then a human is already looking.
///
/// # Never-ticked slots are EXCLUDED, deliberately
///
/// A slot that has never ticked reads `0` and is skipped rather than treated
/// as infinitely stale. Two reasons: an unused slot would peg this gauge at
/// "broken" forever, and a legitimately quiet subscription (a depth-200 socket
/// on one illiquid contract) is not a fault. The never-ticked case has its own
/// signal already — `tv_dhan_feed_instruments_never_ticked` plus RISK-GAP-03 —
/// so it is covered, not dropped.
static PER_CONN_LAST_TICK_MILLIS: [std::sync::atomic::AtomicI64;
    MAX_TOTAL_DHAN_CONNECTIONS as usize] =
    [const { std::sync::atomic::AtomicI64::new(0) }; MAX_TOTAL_DHAN_CONNECTIONS as usize];

/// Stamps that a connection delivered a tick. One relaxed store per FRAME.
///
/// Per frame, not per tick: a frame stacks packets, and the question this
/// answers ("is this socket delivering at all?") is answered identically by
/// either. The cheaper one wins on a path that runs ~5,000 times a second.
///
/// An out-of-range index is dropped rather than panicking or wrapping. The
/// planner cannot produce one — the array is sized from the same constant the
/// pool is — and writing to the wrong slot would report the wrong socket
/// healthy, which is worse than not writing.
pub fn record_connection_tick(connection_index: u8, recv_millis: i64) {
    if let Some(slot) = PER_CONN_LAST_TICK_MILLIS.get(connection_index as usize) {
        slot.store(recv_millis, Ordering::Relaxed);
    }
}

/// Age in seconds of the STALEST connection that has ever ticked.
///
/// `None` when no connection has ticked yet — which is a real state (pre-open,
/// or a lane that has just started) and must NOT render as zero. Zero would
/// mean "every socket ticked this instant", the most reassuring reading
/// available, at the moment we know least.
///
/// Pure, so the boundary cases are testable without a clock.
#[must_use]
pub fn worst_connection_tick_age_secs(now_millis: i64) -> Option<u64> {
    let mut worst: Option<u64> = None;
    for slot in &PER_CONN_LAST_TICK_MILLIS {
        let last = slot.load(Ordering::Relaxed);
        if last == 0 {
            continue; // never ticked — see the type's docs
        }
        // saturating: a clock stepped backwards must read as 0 age, never wrap
        // into a gigantic one at the moment the log is hardest to read.
        let age = u64::try_from(now_millis.saturating_sub(last).max(0) / 1_000).unwrap_or(u64::MAX);
        worst = Some(worst.map_or(age, |w| w.max(age)));
    }
    worst
}

/// Gauge: how stale the WORST-performing live connection is, in seconds.
///
/// `-1` when no connection has ticked yet, so "unknown" is distinguishable
/// from "all fresh" on the chart. A gauge that renders both as 0 is the
/// false-OK shape this repo has retired repeatedly.
pub const WORST_CONN_TICK_AGE_GAUGE: &str = "tv_dhan_ws_worst_conn_tick_age_secs";

/// Gauge: how FULL the ring byte budget is, as a percentage, worst of the two
/// pools.
///
/// # The half of the ring we were not publishing
///
/// The ring is bounded twice — by frame COUNT (65,536) and by BYTES
/// (`RingByteBudget`). `tv_dhan_feed_ring_max_bytes` has been EMF-selected
/// since 2026-08-15, so the CAPACITY reaches CloudWatch and the OCCUPANCY does
/// not: we ship the denominator and withhold the numerator.
/// `RingByteBudget::resident()` existed the whole time with call sites only in
/// its own unit tests — the fifth instance in this lane of code that exists,
/// is tested, and is never invoked.
///
/// # Percent, and worst-of-two
///
/// A percentage rather than raw bytes because the two budgets are sized
/// differently (main-feed and depth split 3:1), so raw bytes are not
/// comparable and a single raw gauge would be dominated by whichever pool is
/// larger regardless of which is in trouble.
///
/// Worst-of-two rather than one gauge per pool for the reason the deaf-socket
/// gauge is worst-of-sixteen: two dimensions cost twice one to answer a
/// question one answers, and per-pool detail stays on `/metrics` where whoever
/// is triaging will already be looking.
///
/// # It pairs with the dwell gauge, and the pair is the diagnosis
///
/// Dwell says how far behind the drain is in TIME; this says how full the ring
/// is in BYTES. Both climbing is a drain that cannot keep up. Dwell flat while
/// this climbs is large frames rather than a slow drain — a different problem
/// with a different fix. Neither number says that alone.
pub const RING_RESIDENT_PCT_GAUGE: &str = "tv_dhan_feed_ring_resident_pct";

/// Fill percentage of one budget, `0.0..=100.0`.
///
/// A zero-capacity budget reads 0.0 rather than dividing — that combination is
/// unreachable in production (both caps are derived from a non-zero host
/// figure) but a NaN reaching a gauge is silently unchartable, and "the chart
/// went blank" is a worse failure than "the chart read zero".
#[must_use]
pub fn budget_fill_pct(resident: usize, cap: usize) -> f64 {
    if cap == 0 {
        return 0.0;
    }
    // `u32::try_from` then `f64::from`, lossless by construction — the house
    // pattern, and the same reason `take_ring_dwell_max_ms` uses it. Ring
    // budgets are gigabyte-scale at most, so the u32 ceiling of ~4.29e9 is
    // never approached; saturating there would read as full, which is the
    // safe direction for a saturation gauge.
    let r = f64::from(u32::try_from(resident).unwrap_or(u32::MAX));
    let c = f64::from(u32::try_from(cap).unwrap_or(u32::MAX));
    (r / c * 100.0).min(100.0)
}
static WS_LAG_HANDLES: OnceLock<WsLagHandles> = OnceLock::new();

fn ws_lag_handles() -> &'static WsLagHandles {
    WS_LAG_HANDLES.get_or_init(WsLagHandles::new)
}

/// What happened to a sealed candle the writer channel would not take.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SealRefusal {
    /// Written to the spill or the DLQ. Recovered by the boot drain.
    Rescued,
    /// Neither disk tier accepted it. The candle is gone.
    Lost,
}

/// Route a seal the writer channel refused to the durable tier, instead of
/// discarding it.
///
/// **This function is the no-drop policy.** Operator directive 2026-08-19:
/// *"never ever drop any ticks irrespective of any worst case"*, and
/// *"never dropped or dleetd dude just mvoe it to db and s3 right?"* Before
/// it existed, all three seal call sites in this file did
/// `if tx.try_send(seal).is_err() { dropped += 1 }` — the sealed candle was
/// counted and thrown away whenever the writer fell behind or was absent. The
/// three-tier ring → spill → DLQ cascade already existed in
/// `seal_absorption`, but only on the CONSUMER side of the channel; nothing
/// on the producer side could reach it. This closes that.
///
/// Ordering is deliberate: spill first (binary, compact, drained on boot),
/// DLQ second (NDJSON, recoverable as text by a human). `Lost` requires BOTH
/// to fail, which means the data volume is unwritable — and the caller fires
/// AGGREGATOR-DROP-01 (Critical, paged) for it.
///
/// Cost, stated honestly: this turns a channel refusal from a free discard
/// into a synchronous disk append on the fold path. That is the intended
/// trade — a slow write is recoverable, a discarded candle is not — and it
/// only happens when the channel is already refusing, never on the happy
/// path.
///
/// The spill/DLQ filename is derived from an IST date, and the seal carries
/// its own bucket-open IST second — so the date comes from the DATA rather
/// than from a clock. That is both cheaper (no syscall on the fold path, per
/// locked decision L-H7) and more correct: a bar sealed at 15:29 that spills
/// at 15:31 belongs to the 15:29 bar's day, and stays on the right side of an
/// IST-midnight boundary no matter when the rescue happens.
#[must_use]
pub fn escalate_refused_seal(seal: &tickvault_trading::candles::BufferedSeal) -> SealRefusal {
    let now_unix_secs = i64::from(seal.state.bucket_start_ist_secs).saturating_sub(i64::from(
        tickvault_common::constants::IST_UTC_OFFSET_SECONDS,
    ));
    let Some(overflow) = tickvault_storage::seal_writer_runner::global_seal_overflow() else {
        // No durable tier installed. Saying "rescued" here would be the exact
        // false-OK this policy exists to prevent, so it is a loss and it is
        // reported as one.
        //
        // 2026-08-19 (same-day hostile audit): this arm used to `return
        // SealRefusal::Lost` with NO log. That is the worst case in the whole
        // seal path, not the mildest: when `SealWriterRunner::new` fails at
        // boot, `main.rs` installs neither the sender nor the overflow, so
        // EVERY seal lands here for the life of the process — and the alarmed
        // drain counter lives inside the writer loop that never spawned, so it
        // reads a flat, healthy zero all day. An entire session of candles
        // could evaporate with nothing paging. It now fires
        // AGGREGATOR-DROP-01.
        crate::seal_loss_alarm::record_lost_seal(
            crate::seal_loss_alarm::SealLossReason::NoDurableTier,
            seal.security_id,
            seal.exchange_segment_code,
            seal.tf.display_name(),
        );
        return SealRefusal::Lost;
    };
    match overflow.escalate(seal, now_unix_secs) {
        tickvault_storage::seal_writer_runner::OverflowOutcome::Spilled
        | tickvault_storage::seal_writer_runner::OverflowOutcome::DlqWritten => {
            SealRefusal::Rescued
        }
        tickvault_storage::seal_writer_runner::OverflowOutcome::Lost => {
            // Both disk tiers refused — the case AGGREGATOR-DROP-01 was
            // written for. The consumer-side triple-failure already pages
            // (`seal_drop_paging_wiring_guard`); this is the PRODUCER side,
            // which had no page at all.
            crate::seal_loss_alarm::record_lost_seal(
                crate::seal_loss_alarm::SealLossReason::BothDiskTiersFailed,
                seal.security_id,
                seal.exchange_segment_code,
                seal.tf.display_name(),
            );
            SealRefusal::Lost
        }
    }
}

/// `pub` so `crates/app/tests/dhat_ws_lag.rs` can measure it from an
/// integration test. The allocation this function must not do is invisible to
/// a unit test; it needs a DHAT profiler, and that needs a test binary.
pub fn record_ws_lag(connection_index: u8, tick: &ParsedTick, received_at_nanos: i64) {
    let handles = ws_lag_handles();
    match ws_lag_ms(tick.exchange_timestamp, received_at_nanos) {
        Some(WsLag::Measured(ms)) => {
            handles.histogram_for(connection_index).record(ms);
        }
        Some(WsLag::ClampedNegative) => {
            handles.histogram_for(connection_index).record(0.0);
            handles.excluded_clamped_negative.increment(1);
        }
        None => {
            handles.excluded_implausible_ltt.increment(1);
        }
    }
}

/// Outcome of [`ws_lag_ms`] for a tick that DOES carry a usable timestamp.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WsLag {
    /// A real, non-negative lag in milliseconds.
    Measured(f64),
    /// The arithmetic came out negative — whole-second truncation, or a host
    /// clock behind the exchange. Recorded as zero and counted, so a clock
    /// problem shows up as a rising counter instead of a silently skewed p50.
    ClampedNegative,
}

/// Re-exported so the drain and the WS classifier walk the SAME packet
/// boundaries. Moved to `core` on 2026-08-25: two walks that disagree let the
/// drain decode a disconnect the classifier never saw, which is exactly how a
/// stacked 804 escaped Fatal classification for a full session.
pub use tickvault_core::parser::dispatcher::main_feed_packet_len;

/// Republishes the fold's depth gauges. Reads only — never mutates the fold.
///
/// Deliberately gauges ONLY, no logging. An earlier draft warned here whenever
/// the refusal total was non-zero — and since this runs every 1,024 frames, a
/// single refusal would have warned every 1,024 frames for the rest of the
/// session. A running total is a gauge's job; the refusal itself already logs
/// once, at the moment it happens.
fn publish_fold_depth(ingest: &LiveIngest) {
    // `u32::try_from` then `f64::from`: lossless by construction, and no lossy
    // `as` cast to justify.
    let pending = f64::from(u32::try_from(ingest.pending_rows()).unwrap_or(u32::MAX));
    let dropped = f64::from(u32::try_from(ingest.seals_dropped()).unwrap_or(u32::MAX));
    let refused = f64::from(u32::try_from(ingest.seq_refused()).unwrap_or(u32::MAX));
    metrics::gauge!(PENDING_ROWS_GAUGE).set(pending);
    metrics::gauge!(SEALS_DROPPED_GAUGE).set(dropped);
    metrics::gauge!(SEQ_REFUSED_GAUGE).set(refused);
    // Ring dwell rides the SAME periodic publish rather than a timer of its
    // own. A second timer would be a second `select!` arm on the drain loop,
    // and the whole point of this measurement is that the drain loop is not
    // being starved — a signal that adds a new way to starve it would be
    // measuring a problem it helped cause.
    metrics::gauge!(RING_DWELL_MAX_MS_GAUGE).set(take_ring_dwell_max_ms());
}

/// The WebSocket base URL for one MARKET-DATA endpoint type.
///
/// `None` for `OrderUpdate` deliberately. That socket is not this lane's to
/// open: `websocket-connection-scope-lock.md` §A.1 retired its spawn, and the
/// module it belongs to is owned by [`crate::dhan_rest_stack`]. Returning a URL
/// here would make it one careless match arm away from being dialed twice.
const fn base_url_for(endpoint: DhanEndpointType) -> Option<&'static str> {
    match endpoint {
        DhanEndpointType::MainFeed => Some(DHAN_MAIN_FEED_WS_BASE_URL),
        DhanEndpointType::Depth20 => Some(DHAN_TWENTY_DEPTH_WS_BASE_URL),
        DhanEndpointType::Depth200 => Some(DHAN_TWO_HUNDRED_DEPTH_WS_BASE_URL),
        DhanEndpointType::OrderUpdate => None,
    }
}

/// Reads the CURRENT Dhan JWT from the process-global token manager.
///
/// A function, not a captured string: the token rotates roughly every 23 hours
/// and the socket must present the live value on every dial, or the
/// supervisor's post-807 re-dial would re-present the dead credential it just
/// got rejected for. The manager is registered by the Dhan REST stack, which
/// mints from AWS SSM — there is no second credential path and nothing is
/// hardcoded here.
fn current_feed_token() -> Option<FeedTokenBuffer<String>> {
    let manager = global_token_manager()?;
    let guard = manager.token_handle().load();
    guard
        .as_ref()
        .as_ref()
        .map(|state| FeedTokenBuffer::new(state.access_token().expose_secret().to_string()))
}

// ---------------------------------------------------------------------------
// Boot wiring
// ---------------------------------------------------------------------------

/// Everything the bring-up needs. Deliberately narrow: the stack takes the ONE
/// config flag it gates on rather than a whole `ApplicationConfig` clone, so
/// wiring it into `main.rs` is a few lines and no deep copy.
pub struct DhanFeedStackParams {
    /// `[feeds] dhan_enabled` from the boot config.
    pub dhan_enabled: bool,
    /// Live-feed frames recovered from the write-ahead log at boot, as
    /// `(frame_seq, raw_bytes)` — the exact bytes a previous session captured
    /// but died before folding.
    ///
    /// # Why these arrive here rather than being folded at the replay site
    ///
    /// Replay happens during boot, in `main.rs`, where the thing that can fold
    /// a frame does not exist yet: `LiveIngest` is constructed on THIS task.
    /// Until 2026-08-15 the replay site therefore counted these frames and
    /// **dropped** them — the largest tick-loss path the system actually owned,
    /// since an unclean stop during market hours discarded every tick captured
    /// since the last flush.
    ///
    /// Handing the batch to the lane is what closes it: the frames are folded
    /// immediately after the ingest exists and before any socket opens, so a
    /// recovered frame can never race a live one.
    ///
    /// Empty on a clean boot, which is the overwhelmingly common case.
    pub wal_replay_live_feed: Vec<(u64, bytes::Bytes)>,
    /// Main-feed instruments (the hardcoded index set — see
    /// [`hardcoded_index_universe`]).
    pub main_feed_instruments: Vec<SubscribeInstrument>,
    /// depth-20 instruments. Empty until an operator-named set exists.
    pub depth_20_instruments: Vec<SubscribeInstrument>,
    /// depth-200 instruments. Empty until an operator-named set exists.
    pub depth_200_instruments: Vec<SubscribeInstrument>,
    /// Where folded ticks are written. Taken by value rather than as a built
    /// `TickWriter` so the writer is constructed on the lane's own task and a
    /// disabled boot never builds one.
    pub questdb: QuestDbConfig,
    /// The process-wide write-ahead log every captured frame lands in BEFORE
    /// it is visible to the fold. `None` refuses the lane: capture-at-receipt
    /// is the durability floor, and a live feed without it would report ticks
    /// as captured that a process kill would erase.
    pub spill: Option<Arc<WsFrameSpill>>,
    /// Whether the REST candle fold is also running and writing Dhan candles.
    ///
    /// Both this lane and the REST fold send sealed candles to the SAME
    /// process-wide seal writer, which lands them in the same `candles_<tf>`
    /// tables stamped `feed='dhan'`. The DEDUP key is
    /// `(ts, security_id, segment, feed)` — every column identical for the
    /// same minute of the same instrument. There is no column that says WHICH
    /// source produced the row, so QuestDB upserts one over the other and the
    /// survivor is whichever wrote last.
    ///
    /// That is bad on its own and worse downstream: the 15:31 comparator reads
    /// `candles_1m WHERE feed='dhan'` as the LIVE side of its check. If
    /// REST-derived rows land there, it compares the REST record against the
    /// REST record — a tautology that always agrees, on the one check that
    /// exists to detect disagreement.
    ///
    /// Separating them properly needs a source discriminator in the key, which
    /// is a schema decision. Until that decision is made, running both is
    /// REFUSED rather than silently corrupted.
    pub rest_fold_writes_dhan_candles: bool,
    /// The operator-facing feed state, so the lane can report whether it is
    /// ACTUALLY running.
    ///
    /// Added 2026-08-14. `set_dhan_lane_running` previously had zero
    /// production callers, which made `feed_health`'s Dhan verdict a constant
    /// — it read "enabled, but the feed was not started at boot" whether the
    /// lane was up, down, or absent. The lane owns that truth, so the lane is
    /// what sets it: `true` once sockets are dialed and the fold is consuming
    /// them, `false` on every exit path.
    pub feed_runtime: Arc<tickvault_api::feed_state::FeedRuntimeState>,
    /// The process-wide feed-health registry, so a DEAD Dhan lane can report
    /// dead.
    ///
    /// Added 2026-08-18. Sibling of `feed_runtime` above, one step further
    /// in: that field fixed the lane's RUNNING flag, but health also needs to
    /// know whether the lane is DELIVERING. `record_ticks` had zero
    /// production callers anywhere in the workspace, so the Dhan verdict
    /// could never fall to `Down` — it answered a benign
    /// `Unknown, "not instrumented yet"` for a corpse.
    ///
    /// The lane owns that truth, so the lane reports it: the drain records
    /// rows FLUSHED TO QUESTDB after every flush. See `flush_and_record` for
    /// why flushed rows are the unit rather than frames received.
    pub feed_health: Arc<tickvault_common::feed_health::FeedHealthRegistry>,
    /// Signalled at process shutdown so the drain can SEAL and FLUSH before
    /// the process exits.
    ///
    /// Added 2026-08-14. Until then the drain could only end when the ring
    /// closed, and nothing closed it — so at every 17:30 stop the day's tail
    /// (open candles, plus every ILP row still under the flush threshold) went
    /// with the process while the log printed "tickvault stopped" and
    /// classified the shutdown clean.
    pub shutdown: Arc<tokio::sync::Notify>,
    /// NSE trading calendar, used to keep the lane's loudest pages off days the
    /// market is shut.
    ///
    /// Added 2026-08-14. The silence detector and the 15:31 cross-verification
    /// both gated on TIME-OF-DAY only, and EventBridge starts this box on
    /// `MON-FRI` — which includes NSE holidays. On such a day the lane dials,
    /// seeds the whole universe, receives nothing (correctly — the market is
    /// shut), and fires `silent=<universe>, never_ticked=<universe>`: the most
    /// alarming page the system can produce, false, several times a year. The
    /// real cost is second-order — it trains the operator to mute the ONE
    /// detector that catches a silently-failed subscribe.
    pub calendar: Arc<tickvault_common::trading_calendar::TradingCalendar>,
}

/// Process-wide handle to the trading calendar, installed by
/// [`spawn_dhan_feed_stack`] from its params.
///
/// A `OnceLock` rather than two threaded parameters because the two consumers
/// (`run_frame_drain` and `spawn_daily_crossverify`) sit in different tasks with
/// different signatures, and this file already uses exactly this shape for
/// `CROSSVERIFY_DEPS`.
static TRADING_CALENDAR: OnceLock<Arc<tickvault_common::trading_calendar::TradingCalendar>> =
    OnceLock::new();

/// True when today is an NSE trading day — or when the calendar is not
/// installed.
///
/// **Fail-OPEN, deliberately.** This gates whether a silence page may fire. The
/// two error directions are not symmetric: a false page is noise, while a
/// SUPPRESSED page on a real trading day is undetected data loss on a lane
/// whose protocol carries no sequence number and no snapshot-on-subscribe, so
/// silence is the only signal there is. If the calendar is somehow absent, page
/// anyway.
fn silence_page_allowed_today() -> bool {
    TRADING_CALENDAR
        .get()
        .is_none_or(|calendar| calendar.is_trading_day_today())
}

/// Brings up the Dhan 16-connection live feed if — and only if — both gates are
/// open. Returns `None` when the lane is disabled or already spawned, which is
/// the overwhelmingly common case and costs one boolean read plus one
/// environment lookup.
///
/// Never blocks boot: the gate and the plan are pure computation, and the
/// bring-up runs on its own task.
// The gate, the universe and the plan are each unit-tested below;
// `test_spawn_dhan_feed_stack_is_refused_when_the_lane_is_disabled` covers the
// disabled path, which is the only path any boot takes today.
/// How often the depth late-attach re-asks for its instrument set.
///
/// The option-chain leg fires once a minute, so a shorter poll only re-reads
/// the same table; a longer one delays depth past the strikes it selected.
pub const DEPTH_ATTACH_RETRY_SECS: u64 = 60;

/// Retry cadence inside the PRE-OPEN readiness window, `[09:00, 09:15)` IST.
///
/// # Why the flat 60s cadence was not enough (operator, 2026-08-22)
///
/// The requirement is that contracts and depth are on the wire by **09:12**,
/// so that 09:15 delivers ticks on everything from the first second. The
/// blocker was never the data — it was the POLL GRID.
///
/// `DEPTH_ATTACH_RETRY_SECS` is a flat 60s sleep whose PHASE is fixed by boot
/// time, so readiness lands on a 60-second grid offset by whenever the box
/// happened to start. Measured on 2026-08-21: boot ~08:30:14, and the attach
/// succeeded on **attempt 38 at 09:08:14** — 38 minutes of one-per-minute
/// polling. That morning it cleared 09:12 by four minutes. It clears by
/// nothing at all if pre-open prices land a minute later: data available at
/// 09:11:20 with a `:50`-phase grid is dialed at **09:12:50**, past the line,
/// for no reason but the sleep.
///
/// Fifteen seconds removes the grid as a failure mode: readiness now trails
/// data availability by at most 15s instead of 59s. Data ready at 09:11:20 is
/// on the wire by 09:11:35 — inside the line — where the 60s grid could have
/// dialed at 09:12:50.
///
/// # Why 15 and not 5 (corrected before merge)
///
/// This constant was first written as 5, costed as "~180 QuestDB reads — a
/// LATEST-ON query, off the tick path". That costing was wrong, and the error
/// is worth recording because it is the shape that matters: I priced the
/// cheapest thing an attempt does and ignored the expensive one.
///
/// Each attempt re-reads and re-parses the DAILY CONTRACT ARTIFACT —
/// `contracts_in_artifact: 121674` in the 2026-08-21 production log, roughly
/// 11 MB of JSON (Estimated: ~90 B/row; the file is not on this machine) — and
/// the contract half of the SAME iteration parses it a second time, plus the
/// ~4,600-row mapping artifact. So an attempt is two large JSON parses, not a
/// database read. At 5s that is ~360 full parses inside the 15-minute window,
/// competing for CPU with the drain that is folding pre-open ticks on a 4-vCPU
/// box. 15s cuts that by two-thirds while still clearing 09:12 with margin.
///
/// # The real fix, deliberately not taken here
///
/// The artifact is immutable once written and keyed by date, so it should be
/// parsed ONCE per day and shared, not per attempt — which would make even a
/// 5s cadence nearly free. That means threading a cache through two loaders
/// that each read the file internally, which is a wider change than the one
/// this commit is making, and it should be measured rather than assumed.
///
/// Deliberately NOT applied all session: after 09:15 nothing is racing a
/// deadline, and a fast poll would turn a benign all-day wait into thousands
/// of parses.
pub const DEPTH_ATTACH_PREOPEN_RETRY_SECS: u64 = 15;

/// IST second-of-day by which contracts and depth must be ON THE WIRE.
///
/// # The requirement (operator, 2026-08-22)
///
/// Verbatim, asked four times: *"9.13 am evryhtign"* — everything subscribed
/// and connected by 09:12 close, so the 09:15 open delivers ticks on every
/// instrument from its first second rather than from whenever the attach
/// happened to finish.
///
/// # What this deadline actually changes
///
/// Nothing in the lane targeted 09:12. `stock_options_are_pending` holds the
/// contract dial until `STOCK_OPTION_PRICING_QUORUM_PERCENT` (60%) of the ~208
/// F&O underlyings have printed a pre-open price, and the ONLY thing that ever
/// cut that wait short was `out_of_time` — which is **10:00 IST**. On
/// 2026-08-21 quorum arrived at 09:08:14 and cleared the line by four minutes;
/// on a thin morning it arrives later and nothing forces the issue for another
/// fifty.
///
/// Past this second the quorum stops being a WAIT and becomes a PREFERENCE:
/// the attach dials whatever is priced.
///
/// # Why it is NOT ORed into `out_of_time`
///
/// `out_of_time` drives two arms, and only one of them wants this. It gates
/// the quorum wait (which should end at 09:12) AND the GIVE-UP arm at
/// `attempts > 0 && out_of_time && !last_had_instruments` (which must NOT).
/// Folding 09:12 into it would turn the deadline into "abandon the session's
/// contracts and depth at 09:12" — the exact opposite of the requirement, on a
/// morning where nothing had resolved yet. A separate flag is the whole
/// difference between "dial what we have" and "dial nothing".
///
/// A source-scan ratchet also pins `out_of_time`'s definition verbatim, so
/// this separation is enforced from two directions.
///
/// 09:12 rather than 09:13: the operator names 09:13 as the moment everything
/// IS ready, so the deadline that produces that state has to fall before it.
/// 09:12:00 leaves a full minute for the subscribe batches to reach Dhan —
/// ~230 messages sent back-to-back with no pacing, so under a second in
/// practice, and the rest is margin.
pub const PREOPEN_READY_DEADLINE_IST_SECS: u32 = 9 * 3_600 + 12 * 60;

/// Whether the pre-open readiness deadline APPLIES to an attach that began at
/// `attach_started_ist_secs`.
///
/// # The false page this closes (MEASURED live, 2026-08-25)
///
/// The deadline verdict was `ready_at <= PREOPEN_READY_DEADLINE_IST_SECS` and
/// nothing else, so it asked "did this attach finish before 09:12?" of EVERY
/// attach — including one that had not started until the afternoon. A restart
/// cannot pass a test whose pass condition is a time already in the past, so
/// the verdict was decided by the clock rather than by anything about the
/// lane. Live that day, on a busy deploy afternoon:
///
/// ```text
/// 09:08:04  attempts: 57  ready_at 32884  deadline 33120  met_deadline: TRUE
/// 12:37:58  attempts:  1  ready_at 45478  deadline 33120  met_deadline: false
/// 16:17:30  attempts:  1  ready_at 58650  deadline 33120  met_deadline: false
/// 17:33:59  attempts:  1  ready_at 63239  deadline 33120  met_deadline: false
/// 18:17:30  attempts:  1  ready_at 65850  deadline 33120  met_deadline: false
/// 19:21:34  attempts:  1  ready_at 69694  deadline 33120  met_deadline: false
/// ```
///
/// The morning MET the deadline with four minutes to spare. Five restarts
/// then each fired `WS-GAP-02` and drove `tv-<env>-preopen-ready-late` into
/// ALARM — armed, since that alarm is ungated by design — so the one alarm
/// built to report a late pre-open reported instead that the box had been
/// redeployed. An alarm that fires on normal operation is the mirror image of
/// the false-OK this repo spends its guards on: it teaches the operator to
/// ignore the one line that would have mattered on a genuinely late morning.
///
/// The alarm's own comment reasoned that "on a restart day the LATEST attach
/// is the one that matters", which is true of a 10:30 re-attach and false of a
/// 19:21 one. The distinction it needed is not WHEN the attach finished but
/// whether it was ever RACING the open, and only the start second carries
/// that.
///
/// Deliberately keyed on the START, not on "is it before the market open":
/// an attach that begins at 09:11 and finishes at 09:20 genuinely missed, and
/// keying on the finish would excuse exactly the case the deadline exists for.
#[must_use]
pub fn preopen_deadline_applies(attach_started_ist_secs: u32) -> bool {
    attach_started_ist_secs < PREOPEN_READY_DEADLINE_IST_SECS
}

/// Gauge: IST second-of-day at which BOTH halves reached the wire.
///
/// # Why this has to exist for the deadline to mean anything
///
/// Before it, nothing in the system measured whether the attach met any
/// particular minute. The requirement "everything subscribed by 09:12" was
/// unfalsifiable from the outside: `late-attach complete` carried an attempt
/// count, not a clock reading, so a 09:08 morning and a 09:47 morning produced
/// the same shape of log line and neither could be alarmed on.
///
/// Published ONCE per session, at the moment the second half dials. Not per
/// attempt — an attempt is not a readiness time — and never on the give-up
/// paths, where there IS no readiness second and publishing a placeholder
/// would read as a met deadline.
pub const PREOPEN_READY_GAUGE: &str = "tv_dhan_preopen_ready_secs";

/// IST second-of-day past which a late-priced stock's options stop being
/// chased — 09:30:00.
///
/// # The gap this exists to close (MEASURED, 2026-08-21, not inferred)
///
/// The contract half dials ONCE and then never re-selects. Live that morning:
///
/// ```text
/// 09:08:14  priced_underlyings: 725   without_spot: 8
///           stock_options: 19442      atm_window: 25   dropped_for_capacity: 0
/// ```
///
/// Eight F&O underlyings had not printed a trade yet, so their ~780 options
/// were absent for the whole session — and nothing ever went back for them.
/// The dial fired on the 60% pricing quorum (725 of 733 is 98.9%), so this
/// predates the 09:12 readiness deadline rather than being caused by it: the
/// deadline changes WHEN the first dial happens, never whether a second look
/// occurs. There was no second look at all.
///
/// 09:30 rather than the session close: an F&O underlying that has not traded
/// in the first fifteen minutes is not going to make its options worth a
/// subscription slot, and an unbounded chase would keep two QuestDB queries
/// and a contract re-selection running all day for nothing.
pub const CONTRACT_TOPUP_CUTOFF_IST_SECS: u32 = 9 * 3_600 + 30 * 60;

/// The most contracts ONE newly-priced underlying can legitimately add:
/// ATM ± 25 strikes, both legs.
///
/// # The permutation this bounds — ATM DRIFT (found by audit 2026-08-22)
///
/// The top-up re-runs the whole contract selection, and that selection
/// re-reads spot prices. A stock whose price crossed a strike boundary since
/// the dial gets a *slid* ATM window: new strikes enter at one edge, and they
/// are not in `sent_contracts`, so they look exactly like a late-priced
/// stock's options to a plain set difference.
///
/// Between the 09:08-ish dial and 09:15 the market OPENS, which is precisely
/// when prices jump — so on a normal Monday this is not a corner case, it is
/// the expected state. Hundreds of already-subscribed stocks can each
/// contribute a strike or two, and the resulting delta would spend the
/// ~2,000 spare slots on re-centering nobody asked for, leaving nothing for
/// the handful of stocks that have NO options at all.
///
/// Re-centering may well be desirable; it is not what #1797 was authorized to
/// do, it was never measured, and it changes capacity behaviour on a loop
/// that has never run in production. So the top-up refuses outright when the
/// delta is bigger than the newly-priced underlyings can account for, and
/// says so with the real numbers. The worst case is that it declines to act —
/// which is exactly the behaviour before #1797, never worse.
pub const MAX_CONTRACTS_PER_LATE_UNDERLYING: usize =
    (2 * tickvault_common::constants::STOCK_OPTION_ATM_STRIKES_EACH_SIDE + 1) * 2;

/// IST second-of-day past which the depth late-attach gives up for the session.
///
/// 10:00 IST — 44 minutes after the option-chain leg's first fire. Past this,
/// a still-empty chain is not "not yet", it is broken, and quietly polling a
/// broken table until 15:30 would report nothing while looking busy.
pub const DEPTH_ATTACH_DEADLINE_IST_SECS: u32 = 10 * 3_600;

/// The minimum time this task always gets, whatever the wall clock says.
///
/// # The live defect this closes (prod evidence, 2026-08-15)
///
/// The wall-clock deadline alone produced exactly ONE doomed attempt on any
/// late start. Measured, from the prod log:
///
/// ```text
/// 10:01:09 IST  tickvault starting
/// 10:01:2x IST  attempt 1 — option_chain_1m empty (the chain leg has not
///               fired yet; the app is 20 seconds old)
/// 10:02:2x IST  attempts > 0 && now >= 10:00  ->  GIVE UP
/// 10:03:26 IST  WS-GAP-02 "gave up ... attempts: 1"
/// ```
///
/// The existing carve-out — "the deadline gates RETRIES, never the FIRST
/// attempt" — was written for a mid-session redeploy, where the table is
/// FULLEST because it has been filling since 09:16. That reasoning is sound
/// and it does not transfer to a late BOX start, where the table is empty for
/// today AND the clock is past the deadline. There the one permitted attempt
/// is taken seconds after boot, before the chain leg has ever run, so it is
/// guaranteed to find nothing — and then the deadline cancels every retry that
/// would have found something a minute later.
///
/// The condition being tested was "is it late?" when the question that matters
/// is "has the chain leg had a chance since WE started?". Those are the same
/// question on a normal morning and opposite questions after a late start,
/// which is why one deadline could not answer both.
///
/// 30 minutes: the chain leg fires once a minute, so any healthy start gets
/// ~30 chances. It is deliberately longer than a QuestDB restart or a
/// token-refresh stall, and short enough that a genuinely broken chain is
/// still declared broken well inside the session.
pub const DEPTH_ATTACH_MIN_WINDOW_SECS: u64 = 30 * 60;

/// IST second-of-day past which depth is not worth attaching at all.
///
/// 15:30 IST — the close. The minimum window above must not be able to keep
/// this task polling into the evening after a 15:25 restart; depth on a closed
/// market subscribes contracts that will not trade again today.
pub const DEPTH_ATTACH_HARD_STOP_IST_SECS: u32 = 15 * 3_600 + 30 * 60;

/// Current IST second-of-day.
///
/// `pub(crate)` since 2026-08-26: the per-minute depth rebalance
/// (`crate::depth_rebalance`) needs the SAME clock this lane uses. A second
/// implementation would be one more place for the IST offset to drift, and a
/// drift there puts every rebalance in the wrong minute.
pub(crate) fn ist_second_of_day_now() -> u32 {
    let now_ist = chrono::Utc::now().timestamp().saturating_add(i64::from(
        tickvault_common::constants::IST_UTC_OFFSET_SECONDS,
    ));
    u32::try_from(now_ist.rem_euclid(i64::from(tickvault_common::constants::SECONDS_PER_DAY)))
        .unwrap_or(0)
}

/// Poll for depth's instrument set and dial its sockets when it arrives.
///
/// See the call site for why this exists at all (the set is published ~45
/// minutes after this stack spawns) and why the sender is weak.
///
/// Fail-CLOSED and LOUD at the deadline: zero depth sockets with a coded error
/// naming the reason, never a silent session that looks configured.
/// Main-feed instrument slots still free after the spot universe.
///
/// Expressed in whole CONNECTIONS rather than instruments, because that is the
/// unit the pool actually allocates. The spot universe is ~4,565 instruments,
/// which is one connection carrying 4,565 of its 5,000 — so 435 slots on that
/// socket are unreachable to a second planning pass, and pretending otherwise
/// would size a contract set that needs a sixth connection the lock forbids.
///
/// Returns 0 when the spot universe already used every connection: the correct
/// answer is then "no contracts", not "squeeze them in somewhere".
/// Which halves of the late attach still need dialing this iteration.
///
/// Extracted as a pure function because it IS the 2026-08-21 defect, and it is
/// the only part of the attach testable without a live pool and a QuestDB. The
/// pre-fix code could not even express the state this returns — a single
/// boolean gated both halves, so "contracts are on the wire, depth still needs
/// to dial" had no representation and the loop simply exited.
///
/// `pending` (see `stock_options_are_pending`) holds the CONTRACT half only.
/// Depth must never be held by it: depth does not read spot prices, so waiting
/// for a stock to print cannot make the option chain arrive any sooner.
#[must_use]
/// The I-P1-11 composite identity, as a hashable key.
///
/// `security_id` ALONE is not unique — that is the whole point of I-P1-11 —
/// and a set keyed on it would treat an index and an option that happen to
/// share a number as the same instrument, silently withholding one of them
/// from a top-up forever.
fn contract_identity(
    instrument: &tickvault_core::websocket::pool_supervisor::SubscribeInstrument,
) -> (u64, u8) {
    (instrument.security_id, instrument.segment.binary_code())
}

/// Subscribe contracts that appeared in a LATER selection onto connections
/// that are already live, and return how many reached the wire.
///
/// # Why a set difference is the load-bearing part
///
/// [`SubscribeGuard::try_extend`] is fail-closed only PAST the per-connection
/// cap; BELOW it, a second send of an instrument the socket already holds is a
/// silent double-subscribe, and Dhan answers an over-limit subscribe with 804
/// by dropping the connection. So the safety of this whole path rests on
/// `delta` being provably disjoint from everything already sent — which a set
/// difference over the composite key gives exactly, not approximately. Nothing
/// here is heuristic: an instrument is sent if and only if no connection has
/// ever been told about it.
///
/// The room figures are the callers' own accounting, decremented as sends
/// succeed. They are an optimisation, not the safety property — a wrong room
/// number costs a refusal from `try_extend`, which leaves the guard untouched.
fn top_up_late_contracts(
    selection: &[tickvault_core::websocket::pool_supervisor::SubscribeInstrument],
    sent: &mut std::collections::HashSet<(u64, u8)>,
    slots: &mut [(
        tokio::sync::mpsc::Sender<
            tickvault_core::websocket::pool_supervisor::LiveSubscriptionCommand,
        >,
        usize,
    )],
    attempts: u32,
    budget: usize,
) -> usize {
    let candidate: Vec<_> = selection
        .iter()
        .filter(|i| !sent.contains(&contract_identity(i)))
        .copied()
        .collect();
    if candidate.is_empty() {
        return 0;
    }
    // A SECOND dedup layer, matching the pool dial. `select_contract_universe`
    // already dedups at source (its `chosen` set), and the pool path gets
    // `dedup_subscribe_set` inside `build_feed_stack_plan` on top of that —
    // but the top-up bypasses the planner and goes straight to `try_extend`,
    // so without this it would run on ONE layer where the dial runs on two.
    // A duplicate inside one payload is a double-subscribe the `sent` filter
    // cannot see, because neither copy is on the wire yet.
    let (delta, duplicates) = dedup_subscribe_set(&candidate);
    if duplicates > 0 {
        error!(
            code = ErrorCode::WsGapSubscriptionBatching.code_str(),
            attempts,
            duplicates,
            "the contract selection produced duplicate instruments inside one top-up payload \
             — deduped before the wire. This should be impossible (the selection dedups at \
             source), so treat it as a selection regression, not a top-up quirk."
        );
    }
    // ATM-DRIFT REFUSAL. See `MAX_CONTRACTS_PER_LATE_UNDERLYING`.
    if delta.len() > budget {
        error!(
            code = ErrorCode::WsGapSubscriptionBatching.code_str(),
            attempts,
            delta = delta.len(),
            budget,
            "REFUSING the whole late top-up: the delta is larger than the newly-priced \
             underlyings can account for, which means it is dominated by ATM windows that \
             SLID as prices moved, not by stocks that had no options at all. Subscribing it \
             would spend the remaining slots on re-centering. Nothing was sent; the session \
             keeps exactly what it dialed."
        );
        return 0;
    }
    let mut cursor = 0usize;
    let mut placed = 0usize;
    for (tx, room) in slots.iter_mut() {
        if cursor >= delta.len() {
            break;
        }
        if *room == 0 {
            continue;
        }
        let take = (*room).min(delta.len() - cursor);
        let chunk = delta[cursor..cursor + take].to_vec();
        match tx.try_send(LiveSubscriptionCommand::Extend(chunk)) {
            Ok(()) => {
                for instrument in &delta[cursor..cursor + take] {
                    sent.insert(contract_identity(instrument));
                }
                *room = room.saturating_sub(take);
                placed = placed.saturating_add(take);
                cursor += take;
            }
            Err(err) => {
                // NOT recorded as sent, and deliberately: a refused send means
                // the socket never heard about these, so the next attempt must
                // be free to offer them again. Recording them here is the one
                // mistake that would turn a transient full channel into a
                // permanent hole.
                warn!(
                    code = ErrorCode::WsGapSubscriptionBatching.code_str(),
                    attempts,
                    offered = take,
                    %err,
                    "a live connection would not accept a late contract top-up — trying the \
                     next connection. Nothing was marked subscribed."
                );
            }
        }
    }
    let unplaced = delta.len().saturating_sub(placed);
    if unplaced > 0 {
        // TWO causes, one message until 2026-08-22 — and they send triage in
        // opposite directions. An EMPTY `slots` means no connection ever
        // registered a top-up channel at all: every dial failed, or the
        // contract half was marked done without leaving a sender behind. That
        // is a wiring failure with nothing to do with capacity, and blaming
        // the 5 x 5,000 budget for it sends the operator hunting an overflow
        // that does not exist. Found by the 2026-08-22 permutation sweep,
        // which asked what this line says when there is nothing to send to.
        let cause = if slots.is_empty() {
            "no live connection ever registered a top-up channel — a WIRING failure, not a \
             capacity one: the contract half reported done without leaving a sender behind"
        } else {
            "every main-feed connection is at its cap, which means the authorized universe no \
             longer fits the 5 x 5,000 budget"
        };
        error!(
            code = ErrorCode::WsGapSubscriptionBatching.code_str(),
            attempts,
            delta = delta.len(),
            placed,
            unplaced,
            connections = slots.len(),
            cause,
            "late-priced contracts had no room on any live connection — they are NOT \
             subscribed this session"
        );
    } else {
        info!(
            attempts,
            placed,
            "late-priced contracts subscribed on the live connections — these are the \
             options of underlyings that had not traded when the contract half first dialed"
        );
    }
    placed
}

fn outstanding_halves(
    depth_resolved: bool,
    contracts_resolved: bool,
    depth_done: bool,
    contracts_done: bool,
    pending: bool,
) -> (bool, bool) {
    (
        contracts_resolved && !contracts_done && !pending,
        depth_resolved && !depth_done,
    )
}

#[must_use]
fn remaining_main_feed_capacity(connections_used: usize) -> usize {
    let max = usize::from(tickvault_core::websocket::pool_budget::MAX_MAIN_FEED_CONNECTIONS);
    let per_connection = usize::try_from(
        tickvault_core::websocket::pool_budget::MAIN_FEED_INSTRUMENTS_PER_CONNECTION,
    )
    .unwrap_or(usize::MAX);
    max.saturating_sub(connections_used)
        .saturating_mul(per_connection)
}

/// Main-feed connections a set of `instruments` occupies.
///
/// # This must mirror `plan_pool`, not the packing arithmetic
///
/// CORRECTED 2026-08-20. This returned `instruments.div_ceil(per_connection)`
/// — the number of connections the set would need if it were PACKED. But
/// `plan_pool` does not pack; it SPREADS, deliberately, taking
/// `min(available, set.len()).max(1)` connections so no socket carries the
/// whole universe. The two answers diverge badly at the live scale:
///
/// | | connections |
/// |---|---|
/// | `plan_pool` dials for 4,565 spots | `min(5, 4565)` = **5** |
/// | this function used to report | `ceil(4565 / 5000)` = **1** |
///
/// So the boot consumed every main-feed slot while the contract attach was
/// told four were free — `remaining_main_feed_capacity(1)` = 20,000 — and
/// asked the pool for room that did not exist. `pool.admit` is stateful and
/// correctly refused, `plan_pool` returned `BudgetRefused`, and because
/// `MainFeed` is the FIRST endpoint in `build_feed_stack_plan`'s loop that
/// error aborted the whole plan **before Depth20 and Depth200 were ever
/// planned**.
///
/// The consequence is the part worth recording: adding contract selection did
/// not merely fail to dial contracts, it took DEPTH DOWN WITH IT — regressing
/// depth from "possible once the chain populates" to "impossible", via an
/// arithmetic disagreement between two functions that never call each other.
/// The retry loop then re-failed identically until the deadline and reported
/// that depth would carry no data this session, which reads as a depth problem
/// and is not one.
///
/// Now mirrors `plan_pool` exactly. When the spot universe has already spread
/// across every connection this returns the cap, `remaining_main_feed_capacity`
/// returns 0, the attach selects no contracts, and the plan SUCCEEDS — so
/// depth dials. That is the honest answer: there is genuinely no room, and
/// saying so lets the sockets that can work, work.
/// `available` is REQUIRED, and that is the whole correction: `plan_pool`
/// spreads across `min(available, len)`, so occupancy is a function of how
/// many connections are free at that moment, not of a global constant. A
/// signature that omitted it could not express the right answer for the
/// second planning pass even in principle.
#[must_use]
fn main_feed_connections_for(instruments: usize, available: usize) -> usize {
    if instruments == 0 || available == 0 {
        return 0;
    }
    let per_connection = usize::try_from(
        tickvault_core::websocket::pool_budget::MAIN_FEED_INSTRUMENTS_PER_CONNECTION,
    )
    .unwrap_or(usize::MAX)
    .max(1);
    // Mirrors `plan_pool`'s MAIN-FEED arm exactly: pack, then clamp to what is
    // actually free. Both halves matter — packing is the policy, and the clamp
    // is what stops this reporting capacity the pool would refuse.
    instruments.div_ceil(per_connection).max(1).min(available)
}

/// Packs an IST `YYYY-MM-DD` date into the `YYYYMMDD` form contract selection
/// compares expiries against.
///
/// Returns 0 on a malformed date, which selects NO contract — every expiry
/// compares as "before today". That is the fail-closed direction: a garbled
/// date must not subscribe an expired contract, whose silence is
/// indistinguishable from a quiet book.
#[must_use]
pub fn ymd_from_ist_date(date_ist: &str) -> u32 {
    let mut parts = date_ist.split('-');
    let (Some(y), Some(m), Some(d)) = (parts.next(), parts.next(), parts.next()) else {
        return 0;
    };
    let (Ok(y), Ok(m), Ok(d)) = (y.parse::<u32>(), m.parse::<u32>(), d.parse::<u32>()) else {
        return 0;
    };
    if !(1970..=2999).contains(&y) || !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return 0;
    }
    y * 10_000 + m * 100 + d
}

#[allow(clippy::too_many_arguments)] // APPROVED: private late-attach task over the boot scope's owned state — bundling would hide which of the two RING BUDGETS each socket gets, and that split is load-bearing (groww_contract_1m_boot precedent)
async fn attach_depth_when_available(
    mut pool: PoolSupervisor,
    questdb: tickvault_common::config::QuestDbConfig,
    client_id: String,
    spill: Arc<WsFrameSpill>,
    frame_weak: tokio::sync::mpsc::WeakSender<CapturedFrame>,
    // Depth's own budget, not the shared one. This path dials ONLY depth
    // sockets, so passing the main feed's share here would silently undo the
    // split it exists to enforce.
    depth_budget: Arc<RingByteBudget>,
    // Since 2026-08-19 this path CAN produce main-feed connections — the
    // contract universe attaches here too, because locating at-the-money needs
    // live prices that do not exist at boot.
    main_feed_budget: Arc<RingByteBudget>,
    // How many main-feed connections the SPOT universe already consumed.
    //
    // `plan_pool` sizes a set against the FULL per-endpoint connection cap and
    // does not know what is already dialed, so without this the contract set
    // would be planned as if all 5 main-feed sockets were free and the pool
    // would be asked for a sixth. The 16-connection lock is arithmetic, not a
    // hope, and this is the term that keeps it so.
    mut main_feed_connections_used: usize,
    // The boot-dialed spot connection's top-up channel, and the room left on
    // it. `None` when the boot dialed nothing (no spot universe) or when the
    // spot set already fills its connection.
    //
    // This is what unstrands the ~4,150 slots. The spot universe (~850) packs
    // onto ONE of five connections; the attach could previously claim only
    // WHOLE free connections, so contracts were capped at 4 x 5,000 = 20,000
    // while ATM +/- 25 needs ~23,000-23,750. Both Dhan caps (5,000 per
    // connection, 5 connections) are already at their documented maximum, so
    // the only slots that exist are the ones already paid for on this socket.
    spot_topup: Option<(
        tokio::sync::mpsc::Sender<
            tickvault_core::websocket::pool_supervisor::LiveSubscriptionCommand,
        >,
        usize,
    )>,
    // Owned, not borrowed: this runs in its own task, hours after boot.
    ws_audit_tx: tokio::sync::mpsc::Sender<
        tickvault_core::websocket::pool_supervisor::WsLifecycleEvent,
    >,
    // Where late-attached instruments go to become VISIBLE to the silence
    // detector. Without it, everything this task dials — the contracts and the
    // depth legs, roughly 80% of the authorized universe — could subscribe and
    // then deliver nothing at all, with no counter, no error, and no way for
    // anyone to find out.
    seed_tx: tokio::sync::mpsc::Sender<
        Vec<tickvault_core::websocket::pool_supervisor::SubscribeInstrument>,
    >,
) {
    // Publish a 0 for every contract-failure reason BEFORE the first attempt.
    //
    // The CloudWatch delta pipeline drops each series' first observed sample as
    // its baseline. `record_contract_verdict` fires ONCE per session, so an
    // un-pre-registered reason would have its first — and only — increment
    // eaten, and the alarm would never see the defect it exists to catch.
    crate::dhan_contract_universe::pre_register_contract_failure_counters();
    crate::dhan_depth_universe::pre_register_depth_failure_counters();

    // The IST second at which this attach sequence BEGAN.
    //
    // This exists to answer one question the deadline verdict below cannot
    // answer for itself: is this attach the PRE-OPEN one at all? See
    // `preopen_deadline_applies`.
    let attach_started_ist = ist_second_of_day_now();
    let mut attempts: u32 = 0;
    // Whether the PREVIOUS attempt resolved something dialable.
    //
    // The give-up arm below used to fire on the deadline alone, which was
    // right when a selection was all-or-nothing. It is not any more: a
    // selection can be genuinely partial — futures and index options
    // resolved, stock options still waiting on a price — and the loop now
    // holds out for the complete one. Holding out has to stop at the
    // deadline, and stopping must mean "dial the partial answer", never
    // "throw it away and dial nothing", or waiting for better would have
    // cost the session the coverage it already had.
    let mut last_had_instruments = false;
    // Which half has already reached the wire. Set ONLY on a successful dial.
    //
    // # The 2026-08-21 defect these two booleans exist to end
    //
    // Depth and contracts shared ONE `anything_resolved` gate and ONE
    // terminating `return`, so whichever half resolved FIRST closed the loop
    // for BOTH. Contracts read `ticks` (filled from 09:15:00) and resolve
    // ~09:15:30; depth reads `option_chain_1m` under a `ts >= today` filter
    // that cannot match before the cadence leg's first fire at 09:16:00.
    // Contracts therefore won that race every single session, the loop
    // returned, and depth-20 + depth-200 never dialed at all — 10 authorized
    // sockets dark, every day, while every alarm read healthy.
    //
    // They must be tracked rather than simply re-attempted: `pool.admit` is
    // STATEFUL, so re-planning an already-dialed half consumes a second set of
    // connection slots.
    let mut contracts_done = false;
    let mut depth_done = false;
    // The contract overflow may be handed to the live spot connection exactly
    // once. `SubscribeGuard::try_extend` refuses only PAST the per-connection
    // cap — below it a second send silently DOUBLE-SUBSCRIBES the same
    // instruments on a live socket, and Dhan answers an over-limit subscribe
    // with 804 and drops the connection.
    let mut spot_topup_used = false;
    // Every contract instrument that has REACHED THE WIRE, by I-P1-11
    // composite key. This is what makes a later top-up safe: see
    // `top_up_late_contracts` for why a set difference, and not a heuristic,
    // is the only thing standing between a late subscribe and an 804.
    let mut sent_contracts: std::collections::HashSet<(u64, u8)> = std::collections::HashSet::new();
    // Top-up channels for connections that are already live, with the room
    // left on each. Populated by the contract dial and by the spot
    // connection's leftover after the initial overflow.
    // One entry per DEPTH connection the attach dials, holding the channel a
    // swap travels down and the instruments that connection was dialed with.
    // Collected here rather than derived later because only the dial knows
    // which connection the pool gave which instruments to.
    let mut depth_commands: Vec<(
        DhanEndpointType,
        tokio::sync::mpsc::Sender<LiveSubscriptionCommand>,
        Vec<SubscribeInstrument>,
    )> = Vec::new();
    let mut live_topups: Vec<(tokio::sync::mpsc::Sender<LiveSubscriptionCommand>, usize)> =
        Vec::new();
    // The contract capacity budget, FROZEN at the first selection.
    //
    // Recomputing it per attempt would be wrong the moment contracts dial:
    // `main_feed_connections_used` grows by the connections they took, so
    // `remaining_main_feed_capacity` collapses from ~24,000 to ~4,000 and
    // `fit_atm_window` would size a completely different (far narrower)
    // window for the top-up selection. The delta would then be computed
    // against a set that never existed, and strikes already on the wire would
    // read as ones to add.
    let mut contract_capacity: Option<usize> = None;
    // The readiness gauge and its deadline verdict fire ONCE per session. The
    // loop can now outlive that moment, so it needs a latch rather than the
    // `return` that used to guarantee it.
    let mut readiness_published = false;
    let mut late_topped_up = 0usize;
    // How many underlyings had NO spot price at the moment contracts dialed.
    // The top-up's budget is derived from how far this figure has since
    // fallen — see `MAX_CONTRACTS_PER_LATE_UNDERLYING`.
    let mut dial_without_spot: Option<usize> = None;
    let started = Instant::now();
    loop {
        // The deadline gates RETRIES, never the FIRST attempt.
        //
        // Checking it before attempt 1 made a mid-session restart give up
        // without even asking: a 12:30 IST redeploy is already past the 10:00
        // deadline, yet that is precisely the moment the chain table is
        // FULLEST — it has been filling since 09:16. Refusing to look would
        // have left depth dark after every intra-day restart, which is the
        // failure this whole task exists to end. Found before deploying,
        // because the deploy that motivated it happened to be mid-session.
        //
        // AND the task always gets DEPTH_ATTACH_MIN_WINDOW_SECS, whatever the
        // clock says. 2026-08-15 prod evidence: an app that started at 10:01
        // IST took its one permitted attempt 20 seconds later — before the
        // chain leg had fired even once — and the wall-clock deadline then
        // cancelled every retry. One doomed look, then dark for the session.
        // "Is it late?" and "has the chain had a chance since we started?" are
        // the same question on a normal morning and opposite ones after a late
        // start; both have to be asked.
        let now_ist = ist_second_of_day_now();
        let window_elapsed = started.elapsed().as_secs();
        let past_hard_stop = now_ist >= DEPTH_ATTACH_HARD_STOP_IST_SECS;
        let past_deadline_and_window = now_ist >= DEPTH_ATTACH_DEADLINE_IST_SECS
            && window_elapsed >= DEPTH_ATTACH_MIN_WINDOW_SECS;

        let out_of_time = past_hard_stop || past_deadline_and_window;

        // DELIBERATELY not folded into `out_of_time` above — see
        // `PREOPEN_READY_DEADLINE_IST_SECS`. This one ends the quorum WAIT; it
        // must never reach the give-up arm, which would read it as "abandon".
        let past_preopen_ready_deadline = now_ist >= PREOPEN_READY_DEADLINE_IST_SECS;

        // A terminal stop that does NOT consult `last_had_instruments`.
        //
        // The give-up arm below deliberately keeps waiting while a half is
        // still resolving. With the two halves now independent, one half can
        // resolve every minute and fail to PLAN every minute — nothing would
        // set `last_had_instruments` false, and the task would poll into the
        // evening for contracts that will not trade again today.
        if attempts > 0 && past_hard_stop {
            error!(
                code = ErrorCode::WsGapSubscriptionBatching.code_str(),
                attempts,
                contracts_done,
                depth_done,
                "late-attach stopped at the 15:30 IST hard stop. Any half showing `false` \
                 above never reached the wire and carries NO data for the rest of this \
                 session."
            );
            // The counter, not just the log. Gated on the half that actually
            // failed: contracts reaching the wire while only depth stayed dark
            // is a DEPTH failure, and paging it as a contract failure would
            // teach the operator to distrust the alarm.
            if !contracts_done {
                crate::dhan_contract_universe::record_contract_give_up();
            }
            return;
        }

        if attempts > 0 && out_of_time && !last_had_instruments {
            error!(
                code = ErrorCode::WsGapSubscriptionBatching.code_str(),
                attempts,
                window_elapsed_secs = window_elapsed,
                ist_second_of_day = now_ist,
                reason = if past_hard_stop {
                    "market close reached"
                } else {
                    "past the 10:00 IST deadline and the minimum window"
                },
                "late-attach gave up: neither the option chain nor the contract artifact \
                 yielded anything, so depth-20 and depth-200 will carry NO data this session \
                 AND the main feed will carry its SPOT universe only — no futures, no option \
                 contracts. Check that the option-chain leg is running with \
                 contract_security_id populated, and that the daily rider wrote today's \
                 contract artifact. If `attempts` is small, this app started late — the chain \
                 leg publishes from 09:16 IST and cannot have run before the app did."
            );
            // Same gate as the hard-stop arm above. Reaching here means
            // NEITHER half ever yielded instruments, so contracts are
            // necessarily incomplete -- checked anyway rather than assumed,
            // because a later edit to the give-up predicate would otherwise
            // silently turn this into a false page.
            if !contracts_done {
                crate::dhan_contract_universe::record_contract_give_up();
            }
            return;
        }
        // Re-derived every attempt, never hoisted: this task can outlive an
        // IST midnight, and a hoisted date would then query yesterday forever.
        let today_date = crate::dhan_universe::today_ist_date();
        let today_nanos = crate::dhan_universe::ist_midnight_nanos(&today_date);
        // Depth prefers the DAILY CONTRACT ARTIFACT over the option chain.
        //
        // Both yield the same thing — a contract `security_id` with a strike,
        // an expiry and a leg — but they become available three minutes
        // apart. The chain's first publish is compile-time asserted to
        // 09:16:00 IST; the artifact is on disk before 08:30. Preferring the
        // artifact is what lets depth-20 and depth-200 carry data from the
        // 09:15 open rather than from 09:16:30.
        //
        // The chain remains the FALLBACK, not a rival: the artifact is
        // written by a separate daily rider, and if that rider had a bad
        // morning, late depth beats no depth.
        //
        // Skipped entirely once depth is on the wire. The loop can now linger
        // past both halves to chase late-priced stocks, and re-running two
        // QuestDB queries a minute for a set that is already subscribed buys
        // nothing. `outstanding_halves` already gates on `depth_done`, so an
        // empty selection here cannot change a decision.
        let selection = if depth_done {
            crate::dhan_depth_universe::DepthSelection::default()
        } else {
            match crate::dhan_depth_universe::load_depth_universe_from_master(
                &questdb,
                &today_date,
                ymd_from_ist_date(&today_date),
            )
            .await
            {
                Some(from_artifact) => from_artifact,
                None => {
                    crate::dhan_depth_universe::load_depth_universe(&questdb, today_nanos).await
                }
            }
        };

        // The FIFTH depth-200 socket: the day's biggest mover.
        //
        // It cannot come from the pair selector. That selector fills in PAIRS
        // and its budget is even precisely so a half-filled pair can never
        // strand a lone leg on an odd socket. So the fifth is appended here,
        // from a different question entirely — which stock has moved furthest
        // today — and only when the four ATM sockets are already accounted
        // for.
        //
        // Appended BEFORE planning, not dialed separately, because `plan_pool`
        // assigns instruments to connections in order: five depth-200
        // instruments become five connections at indices 0..4, and index 4 is
        // exactly `DEPTH_200_TOP_MOVER_SOCKET`. Dialing it afterwards would
        // need the pool a second time, which one task cannot hold twice.
        let mut selection = selection;
        if !depth_done {
            // ONE load for both halves. Two loads a few seconds apart can
            // disagree, and a disagreement here fills the movers sockets from
            // one moment's ranking while the fifth depth-200 socket is chosen
            // from another's.
            let inputs = crate::depth_rebalance::load_attach_inputs(
                &questdb,
                &today_date,
                ymd_from_ist_date(&today_date),
                today_nanos / 1_000,
            )
            .await;

            // ---- depth-20: the operator's layout, when it can be built ----
            //
            // The adaptive selection stays as the FALLBACK, and that ordering
            // is deliberate. Before the chain publishes, the layout has no
            // strikes to centre on and returns nothing; overwriting a working
            // selection with an empty one would trade "the wrong 250" for
            // "no depth at all", which is strictly worse. So the layout is
            // taken only when it actually produced instruments.
            let layout =
                crate::depth20_layout::build_depth20_layout(&inputs.candidates, &inputs.movers);
            if layout.instrument_count() > 0 {
                let flattened = layout.flattened();
                info!(
                    instruments = flattened.len(),
                    sockets = layout.sockets.len(),
                    index_unresolved = layout.index_underlyings_unresolved.len(),
                    movers_unresolved = layout.movers_unresolved,
                    gainers = layout.ranking.gainers.len(),
                    losers = layout.ranking.losers.len(),
                    "depth-20: using the operator layout — index windows plus today's movers"
                );
                selection.depth_20 = flattened;
            } else {
                // Normal before ~09:16, when no chain has published yet.
                tracing::debug!(
                    adaptive_instruments = selection.depth_20.len(),
                    "depth-20: the operator layout has nothing to build from yet — keeping \
                     the adaptive selection for this attempt"
                );
            }

            // ---- depth-200: the fifth socket ----
            //
            // Appended BEFORE planning, not dialed separately, because
            // `plan_pool` assigns instruments to connections in order: five
            // depth-200 instruments become five connections at indices 0..4,
            // and index 4 is exactly `DEPTH_200_TOP_MOVER_SOCKET`. Dialing it
            // afterwards would need the pool a second time, which one task
            // cannot hold twice.
            if selection.depth_200.len() == crate::dhan_depth_universe::DEPTH_200_MAX_SOCKETS {
                match crate::depth_rebalance::top_mover_pick(&inputs.movers, &inputs.candidates)
                    .and_then(|pick| {
                        u64::try_from(pick.leg_security_id())
                            .ok()
                            .filter(|id| *id > 0)
                            .map(|security_id| {
                                tickvault_core::websocket::pool_supervisor::SubscribeInstrument {
                                    security_id,
                                    segment: pick.contract_segment,
                                }
                            })
                    }) {
                    Some(fifth) => {
                        selection.depth_200.push(fifth);
                        info!(
                            security_id = fifth.security_id,
                            "depth-200: the fifth socket takes the day's biggest mover"
                        );
                    }
                    None => {
                        // Normal before the open and on a flat morning: no
                        // stock has a measurable move yet, so there is nothing
                        // to put on it. The retry loop asks again; if the
                        // whole session stays flat the socket simply goes
                        // unused, which is honest.
                        tracing::debug!(
                            "depth-200: no leading mover yet — the fifth socket stays \
                             undialed this attempt"
                        );
                    }
                }
            }
        }

        // The contract universe rides the SAME retry loop, and that is not a
        // convenience — both wait on evidence that only exists after the open
        // (depth on the chain leg's first publish, contracts on the first
        // ticks), and both dial through the pool, which cannot be owned by two
        // tasks at once.
        let contracts = crate::dhan_contract_universe::load_contract_universe(
            &questdb,
            &today_date,
            ymd_from_ist_date(&today_date),
            today_nanos,
            // Whole free connections PLUS the room stranded on the spot
            // connection. Without the second term the attach cannot see
            // ~4,150 already-paid-for slots, and the ATM window silently
            // shrinks to fit a budget that is smaller than the one the
            // operator authorized.
            *contract_capacity.get_or_insert_with(|| {
                remaining_main_feed_capacity(main_feed_connections_used)
                    .saturating_add(spot_topup.as_ref().map_or(0, |(_, spare)| *spare))
            }),
        )
        .await;

        attempts = attempts.saturating_add(1);

        // TWO gates, not one. A single `anything_resolved` let whichever half
        // resolved FIRST close the loop for BOTH — see `contracts_done` /
        // `depth_done` above for why that emptied the depth pools every day.
        let depth_resolved = !selection.depth_20.is_empty() || !selection.depth_200.is_empty();
        let contracts_resolved = !contracts.instruments.is_empty();
        let anything_resolved = depth_resolved || contracts_resolved;
        last_had_instruments = anything_resolved;

        // A selection whose stock options are merely PENDING a price is not
        // an answer yet — see `stock_options_are_pending`. Dialing it would
        // close the loop on a universe missing ~17,000 authorized contracts,
        // which is exactly what happened live on 2026-08-20. Wait, unless
        // waiting has run out of time.
        // ── Why the price-INDEPENDENT contracts wait here too ──────────────
        //
        // Index futures, stock futures and the two full index option chains
        // (~1,380 contracts) need NO spot price — `select_contract_universe`
        // says so at its own class-3 comment, and selects them with an empty
        // price map. So holding them behind a STOCK-pricing quorum looks
        // obviously wrong, and splitting the dial into "price-independent
        // now, stock options later" looks like the fix.
        //
        // It is not. Analysed 2026-08-21 with the real arithmetic, and it
        // makes the session WORSE:
        //
        //   5 main-feed connections x 5,000 = 25,000 slots.
        //   The spot universe (~4,565) packs onto ONE, leaving 4 whole
        //   connections plus ~435 spare on the spot socket ≈ 20,435 for
        //   contracts. The authorized contract set is ~23,820, so it ALREADY
        //   does not fit and `fit_atm_window` shrinks the window to suit.
        //
        //   Dialing the ~1,380 price-independent contracts FIRST gives them a
        //   connection of their own, of which they use 28%. The other ~3,620
        //   slots are then stranded — `plan_pool` cannot retroactively pack a
        //   later set onto a socket already dialed — so stock options lose
        //   ~3,620 slots, which is roughly EIGHT strikes each side off the ATM
        //   window, every day, forever.
        //
        // Trading eight strikes of permanent option coverage for an earlier
        // start on 1,380 futures is a bad trade, and the loss it avoids is
        // bounded anyway: `pending` is ANDed with `!out_of_time` directly
        // below, so the whole set dials at the deadline regardless of whether
        // a single stock ever priced. The wait is bounded; the strike loss
        // would not have been.
        //
        // Recorded because the split is an attractive-looking change that a
        // reader arrives at independently — I did — and the arithmetic that
        // rejects it is not visible from this call site.
        let pending = crate::dhan_contract_universe::stock_options_are_pending(&contracts)
            && !out_of_time
            && !past_preopen_ready_deadline;
        if pending {
            warn!(
                code = ErrorCode::WsGapSubscriptionBatching.code_str(),
                attempts,
                contracts = contracts.instruments.len(),
                underlyings_without_spot = contracts.underlyings_without_spot,
                atm_window_reason = contracts.atm_window_reason,
                "contract attach is holding: the master lists stock options but NO underlying \
                 had a live spot price yet, so at-the-money cannot be located and every stock \
                 option would be absent. Retrying rather than dialing a partial universe — \
                 at the deadline the partial set is dialed anyway rather than dropped."
            );
        }

        let (dial_contracts, dial_depth) = outstanding_halves(
            depth_resolved,
            contracts_resolved,
            depth_done,
            contracts_done,
            pending,
        );

        if dial_contracts || dial_depth {
            // Upgrade LAST, immediately before dialing, and RE-upgraded on
            // every dialing iteration. The two halves can now dial minutes
            // apart, so a ring that closed between them has to be able to stop
            // the second dial as well as the first.
            let Some(frame_tx) = frame_weak.upgrade() else {
                warn!(
                    code = ErrorCode::WsGapConnectionState.code_str(),
                    attempts,
                    contracts_done,
                    depth_done,
                    depth_20 = selection.depth_20.len(),
                    depth_200 = selection.depth_200.len(),
                    "late-attach resolved its instruments but the frame ring had already \
                     closed — the lane went dark while it waited. Refusing to dial sockets \
                     whose frames would reach no consumer."
                );
                return;
            };

            // ---- half 1: the CONTRACT half of the main feed ----
            if dial_contracts {
                // Split the contracts: what the FREE connections can hold goes
                // through the pool as usual; the remainder rides the top-up
                // channel onto the already-live spot connection.
                //
                // The order matters and is deliberate. `select_contract_universe`
                // returns futures and index options FIRST, then the ATM ladders,
                // and it is the ladders that must reach the wire intact — so the
                // OVERFLOW is taken from the tail, leaving the head to the pool.
                let pool_room = remaining_main_feed_capacity(main_feed_connections_used);
                let (pool_contracts, overflow): (&[SubscribeInstrument], &[SubscribeInstrument]) =
                    if contracts.instruments.len() > pool_room {
                        contracts.instruments.split_at(pool_room)
                    } else {
                        (contracts.instruments.as_slice(), &[])
                    };

                // `spot_topup_used` is not belt-and-braces. The channel is a
                // one-shot budget: `try_extend` refuses only PAST the
                // per-connection cap, and below it a second send silently
                // DOUBLE-SUBSCRIBES on a live socket. Dhan answers an
                // over-limit subscribe with 804 and drops the connection.
                if !overflow.is_empty() && !spot_topup_used {
                    match spot_topup.as_ref() {
                        Some((tx, spare)) => {
                            // A bounded send that CANNOT block the attach: the
                            // connection task may be mid-frame, and waiting on
                            // it here would stall the depth dial behind a
                            // socket that is doing its job.
                            match tx.try_send(LiveSubscriptionCommand::Extend(overflow.to_vec())) {
                                Ok(()) => {
                                    spot_topup_used = true;
                                    for instrument in overflow {
                                        sent_contracts.insert(contract_identity(instrument));
                                    }
                                    // Whatever this connection still holds
                                    // after the overflow is real, already-paid
                                    // room. Offering it to the late top-up is
                                    // what retires `spot_topup_used` as a
                                    // one-shot hack: the guard existed only
                                    // because a second send could not be
                                    // proven disjoint from the first, and now
                                    // it can be.
                                    let left = spare.saturating_sub(overflow.len());
                                    if left > 0 {
                                        live_topups.push((tx.clone(), left));
                                    }
                                    info!(
                                        overflow = overflow.len(),
                                        pool_contracts = pool_contracts.len(),
                                        "contract overflow handed to the live spot connection \
                                         — the slots stranded on it are what the ATM window \
                                         was short of"
                                    );
                                }
                                Err(err) => error!(
                                    code = ErrorCode::WsGapSubscriptionBatching.code_str(),
                                    overflow = overflow.len(),
                                    %err,
                                    "the spot connection's top-up channel would not accept \
                                     the contract overflow — those contracts are NOT \
                                     subscribed this session. The pool-dialed contracts and \
                                     depth are unaffected."
                                ),
                            }
                        }
                        None => error!(
                            code = ErrorCode::WsGapSubscriptionBatching.code_str(),
                            overflow = overflow.len(),
                            "contract selection exceeded the free connections and there is no \
                             top-up channel to absorb the remainder — those contracts are NOT \
                             subscribed. This means capacity was budgeted against room the \
                             attach cannot reach."
                        ),
                    }
                }

                // Depth slices deliberately EMPTY here. `build_feed_stack_plan`
                // plans MainFeed FIRST and `plan_pool` returns on the first
                // refusal, so a shared call lets a main-feed budget refusal
                // abort Depth20/Depth200 planning before they are attempted at
                // all. Planning the halves separately is the second half of
                // this fix; `plan_pool` returns Ok immediately on an empty set,
                // so the split costs nothing.
                match build_feed_stack_plan(&mut pool, Instant::now(), pool_contracts, &[], &[]) {
                    Ok(plan) => {
                        let dialed = dial_planned_connections(
                            plan,
                            DialContext {
                                pool: &mut pool,
                                client_id: &client_id,
                                spill: &spill,
                                frame_tx: &frame_tx,
                                main_feed_budget: &main_feed_budget,
                                depth_budget: &depth_budget,
                                ws_audit_tx: Some(&ws_audit_tx),
                                // Retained since 2026-08-22. These connections
                                // are NOT dialed with their final set: eight
                                // F&O underlyings had not traded when this
                                // fired on 2026-08-21, so ~780 of their
                                // options were absent for the whole session
                                // and nothing ever went back for them. Keeping
                                // the senders is what makes going back
                                // possible; `top_up_late_contracts` is what
                                // makes it safe.
                                out_topups: Some(&mut live_topups),
                                out_depth_commands: None,
                            },
                        );
                        // The TERMINAL verdict for today's selection, recorded
                        // once at the moment it reaches the wire. Never per
                        // retry: `no_ladders` before 09:16 is normal, so a
                        // per-attempt emit would page every healthy morning.
                        crate::dhan_contract_universe::record_contract_verdict(&contracts);
                        contracts_done = true;
                        // Only what the POOL carried. The overflow records
                        // itself at its own send site, because it can fail
                        // there — recording the whole selection here would
                        // claim a refused overflow was subscribed and lock
                        // those contracts out of every later top-up.
                        for instrument in pool_contracts {
                            sent_contracts.insert(contract_identity(instrument));
                        }
                        dial_without_spot = Some(contracts.underlyings_without_spot);
                        // Make them VISIBLE to the silence detector, at the
                        // moment they reach the wire and not before. Seeding a
                        // set that failed to dial would report silence for
                        // instruments nobody ever asked for.
                        //
                        // `try_send` rather than `send`: this task must never
                        // block on the drain, and a full 8-slot buffer means
                        // the drain is wedged — which its own alarms cover.
                        if let Err(err) = seed_tx.try_send(contracts.instruments.clone()) {
                            warn!(
                                %err,
                                count = contracts.instruments.len(),
                                "contracts dialed but could not be seeded into the silence \
                                 detector — they will tick normally, but an instrument that \
                                 goes silent among them will not be reported"
                            );
                        }
                        // Keeps `remaining_main_feed_capacity` honest for the
                        // depth half and any later reader. Without it the
                        // used-count still describes boot-only occupancy.
                        main_feed_connections_used =
                            main_feed_connections_used.saturating_add(dialed);
                        info!(
                            dialed,
                            attempts,
                            contracts = contracts.instruments.len(),
                            stock_options = contracts.stock_options,
                            index_options = contracts.index_options,
                            futures = contracts.index_futures + contracts.stock_futures,
                            atm_window = contracts.atm_window_used,
                            depth_done,
                            "late-attach dialed the CONTRACT half of the main feed"
                        );
                    }
                    Err(err) => error!(
                        code = ErrorCode::WsGapSubscriptionBatching.code_str(),
                        ?err,
                        contracts = contracts.instruments.len(),
                        "contract planning refused the selection. RETRYING until the deadline \
                         — a refusal can be transient (a connection budget that frees up). \
                         DEPTH IS PLANNED SEPARATELY AND IS UNAFFECTED by this failure."
                    ),
                }
            }

            // ---- half 2: DEPTH ----
            if dial_depth {
                match build_feed_stack_plan(
                    &mut pool,
                    Instant::now(),
                    &[],
                    &selection.depth_20,
                    &selection.depth_200,
                ) {
                    Ok(plan) => {
                        let dialed = dial_planned_connections(
                            plan,
                            DialContext {
                                pool: &mut pool,
                                client_id: &client_id,
                                spill: &spill,
                                frame_tx: &frame_tx,
                                main_feed_budget: &main_feed_budget,
                                depth_budget: &depth_budget,
                                ws_audit_tx: Some(&ws_audit_tx),
                                out_topups: None,
                                out_depth_commands: Some(&mut depth_commands),
                            },
                        );
                        depth_done = true;
                        // Same as the contract half: depth legs are real
                        // subscriptions and a silently-dead one has no other
                        // evidence.
                        let mut depth_seed = selection.depth_20.clone();
                        depth_seed.extend(selection.depth_200.iter().copied());
                        if let Err(err) = seed_tx.try_send(depth_seed) {
                            warn!(
                                %err,
                                "depth legs dialed but could not be seeded into the silence \
                                 detector"
                            );
                        }
                        info!(
                            dialed,
                            attempts,
                            depth_20 = selection.depth_20.len(),
                            depth_200 = selection.depth_200.len(),
                            contracts_done,
                            "late-attach dialed the DEPTH sockets"
                        );
                    }
                    Err(err) => error!(
                        code = ErrorCode::WsGapSubscriptionBatching.code_str(),
                        ?err,
                        depth_20 = selection.depth_20.len(),
                        depth_200 = selection.depth_200.len(),
                        "depth planning refused its instruments. RETRYING until the deadline \
                         — a refusal can be transient (a connection budget that frees up), \
                         and giving up on the first one would cost the whole session's depth."
                    ),
                }
            }

            // The ONLY success return. Both halves, or keep waiting — the
            // single `return` that used to sit in one shared Ok arm is exactly
            // what left depth dark once contracts won the race.
            if contracts_done && depth_done && !readiness_published {
                // The readiness SECOND, not the attempt count. Published here
                // and nowhere else: this is the only point at which both
                // halves are provably on the wire, and it is reached at most
                // once per session. Read fresh rather than reusing `now_ist`
                // from the top of the iteration — the dials in between take
                // real time, and reporting the second we STARTED looking would
                // flatter every measurement by however long the work took.
                let ready_at = ist_second_of_day_now();
                // Whether the pre-open deadline is a question worth asking of
                // THIS attach — see `preopen_deadline_applies` for the five
                // false pages that made this gate necessary.
                if !preopen_deadline_applies(attach_started_ist) {
                    // A mid-session (re)start. It has a completion second but
                    // no readiness VERDICT: it was never racing the open.
                    //
                    // The field is `attached_at_ist_secs`, NOT
                    // `ready_at_ist_secs`, and that rename is the whole fix on
                    // the alarm side: the CloudWatch metric filter is anchored
                    // on `{ $.fields.ready_at_ist_secs = * }`, so a line that
                    // does not carry that field produces no datapoint and the
                    // alarm stays sparse to genuine pre-open attaches. No
                    // terraform change, and the alarm's threshold semantics
                    // are untouched.
                    //
                    // The gauge is skipped for the same reason its own doc
                    // gives for skipping the give-up paths: there is no
                    // readiness second here, and publishing the wall clock as
                    // one would read as a missed deadline forever after.
                    //
                    // NOT an early `continue`: the late top-up work below runs
                    // on this iteration too, and a mid-session restart is
                    // exactly the shape that most needs it.
                    info!(
                        attempts,
                        attached_at_ist_secs = ready_at,
                        attach_started_ist_secs = attach_started_ist,
                        deadline_ist_secs = PREOPEN_READY_DEADLINE_IST_SECS,
                        "late-attach complete on a mid-session start: contracts and depth are \
                         both on the wire. The pre-open readiness deadline does not apply — \
                         this attach began after it had already passed."
                    );
                } else {
                    metrics::gauge!(PREOPEN_READY_GAUGE).set(f64::from(ready_at));
                    let met_deadline = ready_at <= PREOPEN_READY_DEADLINE_IST_SECS;
                    info!(
                        attempts,
                        ready_at_ist_secs = ready_at,
                        deadline_ist_secs = PREOPEN_READY_DEADLINE_IST_SECS,
                        met_deadline,
                        "late-attach complete: contracts and depth are both on the wire"
                    );
                    if !met_deadline {
                        // An ERROR, not a warning, and deliberately so: everything
                        // subscribed before the open is the stated requirement, and
                        // a session that misses it trades the first minutes without
                        // its contracts. Coded so a metric filter can page on it —
                        // the gauge alone cannot say WHY, and a log line nothing
                        // reads is how the previous deadline went unnoticed.
                        error!(
                            code = ErrorCode::WsGapSubscriptionBatching.code_str(),
                            ready_at_ist_secs = ready_at,
                            deadline_ist_secs = PREOPEN_READY_DEADLINE_IST_SECS,
                            attempts,
                            "late-attach finished AFTER the pre-open readiness deadline — the \
                         session opened without its full contract and depth set on the wire. \
                         Everything dialed, just late."
                        );
                    }
                }
                readiness_published = true;
            }
        }

        // ---- late top-up: the options of stocks that had not yet traded ----
        //
        // Runs only on an iteration that did NOT dial contracts, so the very
        // attempt that fills `sent_contracts` never also scans it.
        if contracts_done && !dial_contracts && !contracts.instruments.is_empty() {
            // The budget is what the underlyings that NEWLY priced can
            // account for. If none has priced since the dial there is nothing
            // legitimate to add, and any delta present is drift — so the
            // top-up does not even look.
            let newly_priced = dial_without_spot
                .unwrap_or(0)
                .saturating_sub(contracts.underlyings_without_spot);
            if newly_priced > 0 {
                late_topped_up = late_topped_up.saturating_add(top_up_late_contracts(
                    &contracts.instruments,
                    &mut sent_contracts,
                    &mut live_topups,
                    attempts,
                    newly_priced.saturating_mul(MAX_CONTRACTS_PER_LATE_UNDERLYING),
                ));
            }
        }

        // The ONLY success return.
        //
        // It used to sit inside the dial block above and fire the instant both
        // halves were on the wire. It now waits out the top-up window — but
        // ONLY when there is a named reason to wait. On a session where every
        // F&O underlying priced before the contract dial,
        // `underlyings_without_spot` is 0 and this returns on exactly the same
        // iteration it always did, having done nothing extra. The loop lingers
        // only to chase instruments it can name.
        if contracts_done && depth_done {
            let chasing = contracts.underlyings_without_spot > 0
                && ist_second_of_day_now() < CONTRACT_TOPUP_CUTOFF_IST_SECS;
            if !chasing {
                info!(
                    attempts,
                    late_topped_up,
                    underlyings_without_spot = contracts.underlyings_without_spot,
                    subscribed_contracts = sent_contracts.len(),
                    "late-attach finished: both halves on the wire and the late top-up window \
                     is closed"
                );
                // Hand the depth-200 channels to the per-minute rebalance
                // BEFORE returning, and hand them by MOVE.
                //
                // This is the line that makes the at-the-money machinery
                // live. Until now `depth_commands` was collected here and
                // dropped on return, which closed every channel the instant
                // the attach finished — so the tracker, the planner and the
                // ranking were all correct and none of them could ever reach
                // a socket.
                //
                // Only Depth200 connections go. A depth-200 connection holds
                // exactly ONE instrument, which is what makes a one-for-one
                // swap meaningful; depth-20 holds up to 50 and needs its own
                // shape, which is a separate change.
                spawn_depth_rebalance(&questdb, &today_date, std::mem::take(&mut depth_commands));
                return;
            }
        }
        // Poll fast while the open is approaching, slow the rest of the day.
        // Re-read per iteration, never hoisted: this loop crosses 09:15, and a
        // value captured once would keep the fast cadence for the session.
        tokio::time::sleep(std::time::Duration::from_secs(preopen_retry_secs(
            ist_second_of_day_now(),
        )))
        .await;
    }
}

/// Turns the attach's dialed depth channels into the rebalance's socket list.
///
/// Pure, and separate from the spawn, because the FILTER is the load-bearing
/// part: only `Depth200` connections belong here. A depth-200 connection holds
/// exactly one instrument, which is what makes a one-for-one swap meaningful.
/// A depth-20 connection holds up to 50, so the same swap would drop one of
/// fifty and add another — a valid wire operation and completely the wrong
/// one, with nothing downstream able to tell.
///
/// A connection dialed with anything other than exactly one instrument is
/// SKIPPED rather than taken with its first: the rebalance's `held` must
/// mirror the wire, and taking one of several would leave the others
/// untracked and the socket ordering wrong for every underlying after it.
#[must_use]
pub fn depth200_rebalance_sockets(
    dialed: Vec<(
        DhanEndpointType,
        tokio::sync::mpsc::Sender<LiveSubscriptionCommand>,
        Vec<SubscribeInstrument>,
    )>,
) -> Vec<crate::depth_rebalance::RebalanceSocket> {
    let mut out = Vec::new();
    for (endpoint, tx, instruments) in dialed {
        if endpoint != DhanEndpointType::Depth200 {
            continue;
        }
        let [only] = instruments[..] else {
            warn!(
                dialed_with = instruments.len(),
                "depth rebalance: a depth-200 connection was dialed with something other \
                 than exactly one instrument, so it is not steerable — a swap names the \
                 instrument it replaces, and that is only unambiguous at one"
            );
            continue;
        };
        out.push(crate::depth_rebalance::RebalanceSocket {
            tx,
            held: Some(only),
        });
    }
    out
}

/// The depth-20 connections, in dial order, with what each actually carries.
///
/// Unlike its depth-200 sibling this takes connections dialed with ANY number
/// of instruments — a depth-20 connection legitimately holds up to 50, and a
/// window that is short near the edge of a freshly-listed expiry holds fewer.
/// A connection dialed with NONE is skipped: an empty socket has no window to
/// move, and a swap naming an instrument it does not hold is refused by the
/// guard anyway.
#[must_use]
pub fn depth20_track_sockets(
    dialed: &[(
        DhanEndpointType,
        tokio::sync::mpsc::Sender<LiveSubscriptionCommand>,
        Vec<SubscribeInstrument>,
    )],
) -> Vec<crate::depth20_track::Depth20LiveSocket> {
    dialed
        .iter()
        .filter(|(endpoint, _, instruments)| {
            *endpoint == DhanEndpointType::Depth20 && !instruments.is_empty()
        })
        .map(
            |(_, tx, instruments)| crate::depth20_track::Depth20LiveSocket {
                tx: tx.clone(),
                held: instruments.clone(),
            },
        )
        .collect()
}
/// Spawns the per-minute depth rebalance for the rest of the session.
// TEST-EXEMPT: spawn wrapper over depth200_rebalance_sockets + run_depth_rebalance, both tested.
fn spawn_depth_rebalance(
    questdb: &tickvault_common::config::QuestDbConfig,
    date_ist: &str,
    dialed: Vec<(
        DhanEndpointType,
        tokio::sync::mpsc::Sender<LiveSubscriptionCommand>,
        Vec<SubscribeInstrument>,
    )>,
) {
    let depth20 = depth20_track_sockets(&dialed);
    let sockets = depth200_rebalance_sockets(dialed);
    if sockets.is_empty() && depth20.is_empty() {
        // Not an error here: a session that dialed NEITHER depth pool has
        // nothing to rebalance, and the loop would say so once and exit.
        // Skipping the spawn says the same thing without a task that exists
        // only to complain.
        //
        // The test is on BOTH pools deliberately. The first version returned
        // on depth-200 alone, which would have killed depth-20 tracking for
        // the whole session any time the depth-200 dial came back empty —
        // two independent pools, one of them silently taken down by the
        // other's bad morning.
        info!("depth rebalance not started: neither depth pool has a steerable connection");
        return;
    }
    let questdb = questdb.clone();
    let date_ist = date_ist.to_owned();
    let today_ymd = ymd_from_ist_date(&date_ist);
    let today_micros = crate::dhan_universe::ist_midnight_nanos(&date_ist) / 1_000;
    tokio::spawn(crate::depth_rebalance::run_depth_rebalance(
        questdb,
        date_ist,
        today_ymd,
        today_micros,
        sockets,
        depth20,
    ));
}
/// How long to wait before the next late-attach attempt, given the IST second.
///
/// Pure so the boundary behaviour is testable without a clock: the whole point
/// is WHICH cadence applies at 08:59:59, 09:00:00, 09:14:59 and 09:15:00, and
/// an inline `if` inside a `tokio::sleep` cannot be asserted at all.
#[must_use]
pub fn preopen_retry_secs(now_ist_secs: u32) -> u64 {
    let from = u64::from(tickvault_common::constants::TICK_PERSIST_START_SECS_OF_DAY_IST);
    let until = CONTINUOUS_SESSION_START_SECS_OF_DAY_IST;
    if (from..until).contains(&u64::from(now_ist_secs)) {
        DEPTH_ATTACH_PREOPEN_RETRY_SECS
    } else {
        DEPTH_ATTACH_RETRY_SECS
    }
}

/// Dial every connection in `plan`, returning how many sockets were opened.
///
/// Extracted from `run_dhan_feed_stack` so BOTH dial phases share one body: the
/// main feed dials at market open, and depth dials later, once the option-chain
/// leg has populated the table depth's instrument set is derived from. Two
/// copies of this loop would be free to drift on the token-refresh closure —
/// the one behaviour that must be identical on every endpoint, because a
/// depth socket that re-dials with a stale JWT after an 807 never recovers.
/// Everything a dial needs that is not the plan itself.
///
/// A struct rather than eight parameters: the list grew past what a reader can
/// hold, and clippy's `too_many_arguments` was the messenger. Grouping them
/// also makes the two call sites — boot and the late attach — differ in exactly
/// the one field that actually differs between them (`out_topups`), instead of
/// in a positional argument nine places along.
struct DialContext<'a> {
    pool: &'a mut PoolSupervisor,
    client_id: &'a str,
    spill: &'a Arc<WsFrameSpill>,
    frame_tx: &'a tokio::sync::mpsc::Sender<CapturedFrame>,
    main_feed_budget: &'a Arc<RingByteBudget>,
    depth_budget: &'a Arc<RingByteBudget>,
    ws_audit_tx: Option<
        &'a tokio::sync::mpsc::Sender<tickvault_core::websocket::pool_supervisor::WsLifecycleEvent>,
    >,
    /// Collects a top-up handle per MAIN-FEED connection dialed: the sender and
    /// the room left on that connection. The boot path passes `Some` so the
    /// later contract attach can reach the slots the spot universe does not
    /// use; the attach passes `None`, because its own connections are dialed
    /// with their final set and have nothing left to add.
    out_topups: Option<&'a mut Vec<(tokio::sync::mpsc::Sender<LiveSubscriptionCommand>, usize)>>,
    /// Collects a command sender for every DEPTH connection dialed, paired
    /// with the instruments that connection was dialed holding.
    ///
    /// **ADDED 2026-08-26** for the per-minute at-the-money re-selection. The
    /// tracker and the swap primitive both existed and had nowhere to send:
    /// depth sockets were dialed with no command channel at all, so a strike
    /// chosen at 09:10 stayed chosen until the session ended.
    ///
    /// The instruments come back with the sender because a swap must name the
    /// OLD one, and only the dial knows which connection got which. Deriving
    /// it later from the selection would be guessing at the pool's packing.
    out_depth_commands: Option<
        &'a mut Vec<(
            DhanEndpointType,
            tokio::sync::mpsc::Sender<LiveSubscriptionCommand>,
            Vec<SubscribeInstrument>,
        )>,
    >,
}

fn dial_planned_connections(plan: FeedStackPlan, ctx: DialContext<'_>) -> usize {
    let DialContext {
        pool,
        client_id,
        spill,
        frame_tx,
        main_feed_budget,
        depth_budget,
        ws_audit_tx,
        mut out_topups,
        mut out_depth_commands,
    } = ctx;
    let mut dialed = 0usize;
    for planned in plan.connections {
        let endpoint = planned.slot.endpoint;
        let Some(base_url) = base_url_for(endpoint) else {
            error!(
                code = ErrorCode::WsGapConnectionState.code_str(),
                endpoint = endpoint.as_str(),
                "the live-feed stack was asked to dial a non-market-data endpoint — refusing. \
                 The order-update socket belongs to the Dhan REST stack and its spawn is \
                 retired (scope-lock §A.1)."
            );
            continue;
        };
        let Some(supervisor) = pool.connection_mut(planned.slot.global_index).map(|s| {
            // Take the supervisor's state by value: `run_connection` drives one
            // connection for its whole life and must own its policy object.
            core::mem::replace(s, ConnectionSupervisor::new(planned.slot, Instant::now()))
        }) else {
            warn!(
                code = ErrorCode::WsGapConnectionState.code_str(),
                endpoint = endpoint.as_str(),
                pool_index = planned.slot.pool_index,
                "planned connection has no registered supervisor — skipping it rather than \
                 dialing an unsupervised socket"
            );
            continue;
        };

        let socket = DhanFeedSocketImpl::new(
            DhanSocketParams::new(endpoint, base_url.to_string(), client_id.to_string()),
            current_feed_token,
        );
        // Endpoint decides the budget. Depth may exhaust depth; it may not
        // evict the feed that actually carries ticks.
        let budget = match endpoint {
            DhanEndpointType::MainFeed => main_feed_budget,
            _ => depth_budget,
        };
        let sink = WalRingSink::new(
            Arc::clone(spill),
            frame_tx.clone(),
            Arc::clone(budget),
            WsType::LiveFeed,
            endpoint,
            // `global_index` (0..16), not `pool_index` — pool indices repeat
            // across endpoints, so labelling by them would merge main-feed
            // socket 0 with depth-20 socket 0 and hide which one is sick.
            planned.slot.global_index,
        );
        // Forensic lifecycle audit, per socket (2026-08-20).
        //
        // Until now `ws_event_audit` had exactly ONE production producer — the
        // order-update socket — so the table covered 1 of the 16 authorized
        // connections. It was not empty, which is what made it dangerous: an
        // operator asking "did any feed socket drop today?" got rows back, saw
        // no live-feed rows, and read that as no drops rather than as not
        // recorded. Counters and alarms did cover the reconnect and park
        // transitions, so this closed a forensics gap rather than a blind spot
        // — but "which socket, when, and why" had no queryable answer.
        let sink = match ws_audit_tx {
            Some(tx) => sink.with_audit(tx.clone()),
            None => sink,
        };
        let sink = Arc::new(sink);
        let guard = planned.guard;
        // Count it alive BEFORE the task starts, so the gauge can never read
        // high because a spawn lost a race with its own decrement.
        let alive = AliveConnectionGuard::acquire();
        // A top-up channel only for MAIN-FEED connections whose caller asked
        // for one. Capacity 1: the attach sends at most one overflow batch per
        // session, and a bounded channel means a wedged connection task
        // surfaces as a refused `try_send` the attach LOGS, never as an
        // unbounded queue that hides it.
        let topup_rx = match (endpoint, out_topups.as_deref_mut()) {
            (DhanEndpointType::MainFeed, Some(sink_vec)) => {
                let spare = guard.spare_capacity();
                let (tx, rx) = tokio::sync::mpsc::channel(1);
                sink_vec.push((tx, spare));
                Some(rx)
            }
            _ => None,
        };
        // Depth sockets get their OWN channel, and it stays open for the
        // session rather than being consumed once. A top-up is a single event
        // — the contract overflow — so its channel is depth 1 and its sender
        // is dropped after. A swap channel must survive every minute of the
        // day, so the sender is held by the re-selection task and the depth
        // is 4: enough that a busy minute cannot block the sender, small
        // enough that a wedged connection surfaces as a refused try_send the
        // caller LOGS rather than as a queue that hides it.
        let topup_rx = match (endpoint, topup_rx, out_depth_commands.as_deref_mut()) {
            (DhanEndpointType::Depth20 | DhanEndpointType::Depth200, None, Some(depth_vec)) => {
                let (tx, rx) = tokio::sync::mpsc::channel(4);
                let held: Vec<SubscribeInstrument> = guard.batches().flatten().copied().collect();
                depth_vec.push((endpoint, tx, held));
                Some(rx)
            }
            (_, existing, _) => existing,
        };
        tokio::spawn(async move {
            // Moved in, so the socket's lifetime and the guard's are the same
            // object. Whatever ends this task — a clean return, an early
            // return, or an unwind — the count comes back down.
            let alive = alive;
            let exit = run_connection_with_commands(
                socket,
                supervisor,
                guard,
                sink,
                || async {
                    // Post-807/809 re-dial: ask the token manager for a fresh JWT
                    // before presenting a credential again. Failure is logged by
                    // the manager and left to the reconnect ladder — re-dialing
                    // with the stale token is the supervisor's own next step and
                    // it will park after the ladder is exhausted.
                    if let Some(manager) = global_token_manager()
                        && let Err(err) = manager.force_renewal().await
                    {
                        warn!(
                            code = ErrorCode::WsGapConnectionState.code_str(),
                            %err,
                            "Dhan live feed could not refresh its token before re-dialing"
                        );
                    }
                },
                topup_rx,
            )
            .await;
            // `run_connection` returning means this socket is GONE — parked,
            // errored, or shut down. Nothing re-dials it, and its shard of the
            // universe stops delivering. Publishing here is what turns
            // "sockets we planned" into "sockets that exist".
            //
            // Via a DROP GUARD, not a bare decrement — 2026-08-20. The
            // increment happens outside this task (deliberately, so the gauge
            // cannot read low because a spawn lost a race), so a decrement
            // written as a plain statement is skipped on unwind and the gauge
            // stays permanently overstated: it would report N sockets alive
            // when N−1 are, with nothing to correct it. Release builds are
            // `panic = "abort"`, so today that shape only bites in debug and
            // test — but the asymmetry is the defect, and a guard costs
            // nothing to make it structural.
            let remaining = alive.release();
            info!(
                endpoint = endpoint.as_str(),
                pool_index = planned.slot.pool_index,
                alive_connections = remaining,
                ?exit,
                "supervised Dhan live-feed connection finished"
            );
        });
        dialed = dialed.saturating_add(1);
    }
    dialed
}

/// What a WAL re-fold actually recovered.
///
/// `refolded` and `lost` are mutually exclusive per tick — a tick is folded XOR
/// lost, never both — so `refolded + lost` is the total the batch contained and
/// neither number can flatter the other.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct WalRefoldOutcome {
    /// Ticks successfully folded into the aggregator and queued for the DB.
    pub refolded: u64,
    /// Ticks parsed but REFUSED by the fold (sequence unrepresentable,
    /// aggregator refusal, write failure). Real, counted loss.
    pub lost: u64,
    /// Frames whose bytes could not be parsed at all.
    pub unparseable: u64,
}
/// Reports live-feed frames the lane recovered from the write-ahead log but
/// will never fold, because bring-up refused before reaching the re-fold.
///
/// # Why this has to exist
///
/// `main.rs` decides whether to drop-and-log these frames itself by asking
/// `feed_stack_gate` whether the lane will run. That gate reads config and one
/// env var — it is the same gate the lane uses, so the two can never disagree
/// about *enablement*. What it cannot see is a RUNTIME refusal: an unplannable
/// pool, a `[rest_candle_fold]` collision, missing cross-verify deps, a missing
/// WAL, or a token manager that never registered. On any of those, `main.rs`
/// has already skipped its own drop-and-log (it believed the lane would fold),
/// and `confirm_replayed` has already MOVED the segments into the archive so
/// they cannot re-stage next boot.
///
/// The frames are therefore gone from the replay path with nobody saying so —
/// captured ticks, silently unrecoverable-by-automation. A silent loss is the
/// one outcome this whole capture chain exists to prevent, so every refusal
/// path that returns before the re-fold calls this first.
///
/// This does not save the frames. The raw bytes remain on disk in the WAL
/// archive (`confirm_replayed` moves, never deletes) and can be recovered by
/// hand; what this restores is the operator's ability to KNOW that is needed.
///
/// The gate-disabled path in `spawn_dhan_feed_stack` deliberately does NOT call
/// this: `main.rs` reads the identical gate and has already logged the drop
/// itself, so reporting again would double-count the same frames.
pub fn report_unfolded_wal_frames(frames: &[(u64, bytes::Bytes)], refusal: &str) {
    if frames.is_empty() {
        return;
    }
    let dropped = frames.len() as u64;
    error!(
        code = ErrorCode::WsGapConnectionState.code_str(),
        frames = dropped,
        refusal,
        "{dropped} live-feed frame(s) were recovered from the write-ahead log but will NOT be \
         folded — the live lane refused to start ({refusal}), and the write-ahead log segments \
         were already archived, so they will not be offered again on the next boot. The ticks \
         and candles they contain are NOT in the database. The raw frames are preserved in the \
         write-ahead log archive and can be recovered manually. If this session followed an \
         unclean stop during market hours, this is real data loss for that window."
    );
    metrics::counter!(
        "tv_ws_frame_wal_reinjected_dropped_total",
        "ws_type" => "live_feed"
    )
    .increment(dropped);
}

/// Re-folds live-feed frames recovered from the write-ahead log.
///
/// # Why this is safe to run twice
///
/// It is idempotent by construction, and the reason is worth stating precisely
/// because it is the whole basis for replaying at all. The `ticks` DEDUP key is
/// `(ts, security_id, segment, capture_seq, feed)`. `capture_seq` is derived
/// from the frame sequence stored IN the WAL record — it is read back, never
/// re-stamped — so a frame folded twice produces byte-identical keys and the
/// second write collapses onto the first instead of duplicating a tick.
///
/// The packet index is folded in for the same reason it is on the live path:
/// one frame can carry many packets, and two trades on the same instrument in
/// the same second would otherwise share a key and silently become one tick.
///
/// # Why it walks packets rather than parsing once
///
/// A single WebSocket message stacks up to ~1,600 packets. Parsing only the
/// first would silently discard the rest — which would make this recovery path
/// quietly lossy, the exact failure it exists to fix.
///
/// # Errors
///
/// Never returns an error. An unparseable frame is counted and skipped rather
/// than aborting the batch, because one corrupt frame must not cost the
/// recovery of every other frame beside it.
pub fn refold_wal_frames(
    ingest: &mut LiveIngest,
    frames: &[(u64, bytes::Bytes)],
) -> WalRefoldOutcome {
    let mut out = WalRefoldOutcome::default();

    // Recovered frames are historical: their exchange timestamps are whatever
    // the exchange stamped, and the receipt instant is gone with the dead
    // process. Using "now" is deliberate and only affects the delivery-lag
    // reading, which is meaningless for a replayed frame — the tick's own
    // exchange timestamp, which decides the candle bucket, is read from the
    // packet exactly as on the live path.
    let received_at_nanos = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);
    let recv_millis = u64::try_from(received_at_nanos.max(0) / 1_000_000).unwrap_or(0);

    for (frame_seq, bytes) in frames {
        let mut offset = 0usize;
        let mut packets = 0u32;
        while offset < bytes.len() {
            let Some(len) = main_feed_packet_len(&bytes[offset..]) else {
                // Stop at the first unrecognised boundary rather than
                // resynchronising on a guess, which would fabricate ticks.
                out.unparseable = out.unparseable.saturating_add(1);
                break;
            };
            let end = offset.saturating_add(len);
            if end > bytes.len() {
                out.unparseable = out.unparseable.saturating_add(1);
                break;
            }
            if let Ok(ParsedFrame::Tick(tick) | ParsedFrame::TickWithDepth(tick, _)) =
                dispatch_frame(&bytes[offset..end], received_at_nanos)
            {
                // Deliberately NOT calling `record_ws_lag`: a replayed frame
                // has no meaningful delivery latency, and feeding "now minus
                // the exchange stamp" into the lag histogram would poison the
                // one number the operator uses to judge the live feed.
                match ingest.ingest_tick_at(&tick, *frame_seq, packets, recv_millis) {
                    // `WrittenOutOfSession` counts as RECOVERED, not lost, and
                    // the distinction is the whole point of the variant: the
                    // row reached the writer, only the candle was skipped
                    // because the tick falls outside the aggregating session.
                    //
                    // **CORRECTED 2026-08-28: this named "(the 09:00-09:15
                    // pre-open)" as the example, and that window is now IN
                    // session.** The candle grid opens at 09:00, so a
                    // 09:00-09:14 tick folds into a real pre-open bar and this
                    // arm is no longer reached for it. What DOES reach here is
                    // a tick before 09:00 or at/after 15:40 — the persistence
                    // window is wider than the candle window at both ends.
                    // Third stale-reachability comment corrected in this
                    // change; a reachability claim is one grep of the gate
                    // constant and must be re-run when written, not carried.
                    //
                    // Counting it as `lost` would
                    // report real recovered rows as data loss; folding it into
                    // `refolded` silently would erase the fact that no bar was
                    // produced. It is a row, so it belongs on the row side.
                    //
                    // This arm exists because two sessions' work met here: the
                    // re-fold arrived from one branch, the out-of-session split
                    // from another, and the exhaustive match is what forced the
                    // question to be answered rather than defaulted.
                    IngestOutcome::Folded { .. } | IngestOutcome::WrittenOutOfSession => {
                        out.refolded = out.refolded.saturating_add(1);
                    }
                    IngestOutcome::SeqUnrepresentable
                    | IngestOutcome::AggregatorRefused
                    | IngestOutcome::WriteFailed => {
                        out.lost = out.lost.saturating_add(1);
                    }
                }
            }
            packets = packets.saturating_add(1);
            offset = end;
        }
    }

    metrics::counter!("tv_dhan_wal_refolded_total", "outcome" => "refolded")
        .increment(out.refolded);
    metrics::counter!("tv_dhan_wal_refolded_total", "outcome" => "lost").increment(out.lost);

    out
}

pub fn spawn_dhan_feed_stack(params: DhanFeedStackParams) -> Option<tokio::task::JoinHandle<()>> {
    let env_opt_in = std::env::var(DHAN_LIVE_FEED_ENV).ok();
    let gate = feed_stack_gate(params.dhan_enabled, env_opt_in.as_deref());
    if !gate.is_enabled() {
        // Debug, not info: this is the normal state on every boot since
        // 2026-07-13 and must not add a line to an operator's log.
        tracing::debug!(
            gate = gate.as_str(),
            "Dhan 16-connection live feed not enabled — no socket opened, no task spawned"
        );
        return None;
    }
    if FEED_STACK_SPAWNED.swap(true, Ordering::SeqCst) {
        warn!(
            code = ErrorCode::WsGapConnectionState.code_str(),
            "Dhan live-feed stack already spawned this process — refusing a duplicate. Two \
             stacks would share one sixteen-socket budget and Dhan answers an over-limit \
             pool by silently killing its OLDEST socket."
        );
        report_unfolded_wal_frames(&params.wal_replay_live_feed, "duplicate_spawn");
        return None;
    }
    // ARM the `/health` websocket row with the count as it stands right now
    // (zero -- no socket has dialed yet).
    //
    // Without this the row only arms on the FIRST successful dial, so a lane
    // that is enabled and dials NOTHING -- empty universe, every endpoint
    // refused, credentials rejected -- reads `retired`: the same answer a box
    // with the feature switched off gives. That is the worst permutation of
    // the three, because it is the one where something is actually wrong.
    // Armed here it reads `disconnected` with a count of 0, and
    // `overall_status` degrades, which is the truth.
    //
    // Deliberately routed through `publish_alive_connections` rather than
    // pushing 0 directly: one function owns the health push, so there is no
    // second path to drift.
    //
    // Installed BEFORE that call, so the arming push below is the FIRST thing
    // to reach the registry. Ordering matters: install after it and the very
    // first transition — the one that arms the row — is silently dropped, and
    // the row keeps its boot-time `false` until the next socket event. On a
    // lane that dials once and stays up, "the next socket event" is the 17:30
    // shutdown.
    if !install_feed_health(Arc::clone(&params.feed_health)) {
        // Already installed. `FEED_STACK_SPAWNED` makes a second spawn
        // impossible in production, so this can only be a repeat call under
        // test; the first registry stays authoritative either way.
        tracing::debug!("Dhan lane feed-health registry was already installed — keeping the first");
    }
    publish_alive_connections(ALIVE_CONNECTIONS.load(Ordering::SeqCst));

    // Installed BEFORE the bring-up task is spawned, so neither the silence
    // detector nor the 15:31 cross-verification can ever observe an empty cell
    // and fall back to its fail-open branch on a real trading day.
    if TRADING_CALENDAR.set(Arc::clone(&params.calendar)).is_err() {
        // Already installed. `FEED_STACK_SPAWNED` above makes a second spawn
        // impossible in production, so this can only be a repeat call under
        // test; the first calendar stays authoritative either way.
        tracing::debug!("Dhan lane trading calendar was already installed — keeping the first");
    }
    Some(tokio::spawn(run_dhan_feed_stack(params)))
}

/// The bring-up body: plan, reserve, publish, report. Never panics, never
/// halts the process, never blocks boot.
// Private bring-up body: every decision it makes (gate, universe, plan,
// budget) is unit-tested below, and it performs no I/O of its own.
async fn run_dhan_feed_stack(params: DhanFeedStackParams) {
    // Rule 11: the lane reads DOWN until it is provably carrying data, so a
    // half-wired stack is never presented as up.
    metrics::gauge!(FEED_STACK_UP_GAUGE).set(0.0);

    let mut pool = PoolSupervisor::new();
    let plan = match build_feed_stack_plan(
        &mut pool,
        std::time::Instant::now(),
        &params.main_feed_instruments,
        &params.depth_20_instruments,
        &params.depth_200_instruments,
    ) {
        Ok(plan) => plan,
        Err(err) => {
            error!(
                code = ErrorCode::WsGapConnectionState.code_str(),
                %err,
                "Dhan live-feed stack could not be planned — no socket opened this session"
            );
            report_unfolded_wal_frames(&params.wal_replay_live_feed, "plan_failed");
            return;
        }
    };

    for endpoint in DhanEndpointType::ALL {
        let planned = plan.count_for(endpoint);
        // `u32::try_from` then `f64::from`: lossless by construction (the count
        // is bounded by 16), and no lossy `as` cast to justify to clippy.
        let planned_f64 = f64::from(u32::try_from(planned).unwrap_or(u32::MAX));
        metrics::gauge!(FEED_STACK_CONNECTIONS_GAUGE, "endpoint" => endpoint.as_str())
            .set(planned_f64);
    }

    info!(
        planned_connections = plan.len(),
        main_feed = plan.count_for(DhanEndpointType::MainFeed),
        depth_20 = plan.count_for(DhanEndpointType::Depth20),
        depth_200 = plan.count_for(DhanEndpointType::Depth200),
        budget_open = pool.total_open(),
        "Dhan 16-connection live feed planned (operator authorization 2026-08-09)"
    );

    // ---- exclusivity floor -------------------------------------------------
    // Two writers into one table under a key that cannot tell them apart is
    // silent data loss, so this refuses rather than corrupts. See the
    // `rest_fold_writes_dhan_candles` field docs for the full reasoning.
    if params.rest_fold_writes_dhan_candles {
        error!(
            code = ErrorCode::WsGapConnectionState.code_str(),
            "Dhan live feed is enabled while the REST candle fold is ALSO writing Dhan \
             candles — REFUSING to open any socket. Both write sealed candles into the same \
             candles_<tf> tables stamped feed='dhan', and the dedup key cannot tell them \
             apart, so one silently overwrites the other. It would also make the 15:31 \
             cross-verification compare the REST record against itself and always agree. \
             Turn OFF [rest_candle_fold] to run the live lane, or leave the live lane off."
        );
        report_unfolded_wal_frames(&params.wal_replay_live_feed, "rest_candle_fold_collision");
        return;
    }

    // ---- verification floor ------------------------------------------------
    // The 15:31 cross-verify is BLOCKING, not optional: the main feed has no
    // snapshot-on-subscribe and no sequence number, so packet loss is
    // invisible at the protocol level and this comparator is the only ground
    // truth the lane has.
    //
    // 2026-08-11: this block previously CALLED `spawn_daily_crossverify` and
    // discarded the result, one line below a comment asserting the check was
    // "BLOCKING, not optional ... it can never be enabled without its own
    // verifier". Both halves were false. Nothing registered the comparator's
    // dependencies, so it always took its refusal branch, and the lane opened
    // all sixteen sockets regardless — capturing data it had no way to verify
    // while the comment said that was impossible.
    //
    // It is now a real refusal, in the same shape as the WAL floor below.
    // Ordered FIRST among the three because it is the cheapest to satisfy and
    // the most expensive to discover missing: a lane with no WAL loses ticks
    // visibly on the next restart, whereas a lane with no verifier looks
    // perfect right up until someone compares it against the broker's record.
    let Some(crossverify) = spawn_daily_crossverify(&params.main_feed_instruments) else {
        error!(
            code = ErrorCode::WsGapConnectionState.code_str(),
            planned_connections = plan.len(),
            "Dhan live feed is enabled but the 15:31 cross-verification could not be armed — \
             REFUSING to open any socket. This feed carries no sequence number and no \
             snapshot-on-subscribe, so the daily comparison against Dhan's own REST record is \
             the ONLY way packet loss can ever be detected. Capturing without it would produce \
             data that cannot be verified, and a missing minute would be indistinguishable \
             from a quiet one. Call install_crossverify_deps() during boot, before this stack \
             spawns."
        );
        report_unfolded_wal_frames(&params.wal_replay_live_feed, "crossverify_deps_missing");
        return;
    };
    // Held for the lane's lifetime so the comparator cannot be dropped while
    // sockets are still capturing.
    let _crossverify = crossverify;

    // ---- capture floor -----------------------------------------------------
    // Refused, not degraded: a live feed with no write-ahead log would report
    // frames as captured that a process kill erases. Fail-closed is the only
    // honest direction when the operator's standing requirement is that not a
    // single tick is missed.
    let Some(spill) = params.spill else {
        error!(
            code = ErrorCode::WsGapConnectionState.code_str(),
            planned_connections = plan.len(),
            "Dhan live feed is enabled but no write-ahead log was supplied — REFUSING to open \
             any socket. Capture-at-receipt is the durability floor; without it a process kill \
             would silently erase every frame received since the last flush."
        );
        report_unfolded_wal_frames(&params.wal_replay_live_feed, "wal_missing");
        return;
    };

    // The client id is a credential-adjacent value the token manager owns. No
    // manager means there is no JWT to dial with — refuse rather than dial
    // with a blank.
    //
    // WAIT for it, do not race it. This was a bare `else { return }`, and it
    // lost that race on essentially every boot.
    //
    // The registrar is `dhan_rest_stack`, which registers the manager only
    // AFTER `TokenManager::initialize` — SSM credential reads, a TOTP
    // computation, and an HTTPS `generateAccessToken` round-trip, inside a
    // retry loop with a >=130s backoff floor. This lane is spawned from the
    // same boot path with exactly ONE await between the two (a localhost
    // QuestDB GET for the depth universe, single-digit milliseconds). A
    // localhost query cannot outlast a remote auth handshake, so the lane
    // reached this line first, refused, and returned — permanently for that
    // process, because nothing re-checked.
    //
    // The refusal was loud, which is the only reason this was recoverable at
    // all. But a lane that logs an error and exits on every single boot is
    // indistinguishable, in effect, from a lane that was never wired.
    let mut client_id: Option<String> = None;
    for attempt in 0..TOKEN_MANAGER_WAIT_ATTEMPTS {
        if let Some(id) = global_token_manager().map(|m| m.client_id_string()) {
            if attempt > 0 {
                info!(
                    waited_secs = attempt * TOKEN_MANAGER_WAIT_INTERVAL_SECS,
                    "Dhan live feed: token manager registered — proceeding to dial"
                );
            }
            client_id = Some(id);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_secs(
            TOKEN_MANAGER_WAIT_INTERVAL_SECS,
        ))
        .await;
    }
    let Some(client_id) = client_id else {
        error!(
            code = ErrorCode::WsGapConnectionState.code_str(),
            waited_secs = TOKEN_MANAGER_WAIT_ATTEMPTS * TOKEN_MANAGER_WAIT_INTERVAL_SECS,
            "Dhan live feed is enabled but no token manager registered within the wait budget, \
             so there is neither a client id nor a JWT to dial with. REFUSING to open any \
             socket. The Dhan REST stack registers the manager after authentication — this \
             means authentication did not complete in time (check AUTH-GAP-* and DH-901)."
        );
        report_unfolded_wal_frames(&params.wal_replay_live_feed, "token_manager_missing");
        return;
    };

    // ---- the fold ----------------------------------------------------------
    // DISTINCT slots across all three pools — not the sum of their lengths.
    // See `distinct_fold_slots` for why summing double-counts an instrument
    // that is on both the main feed and a depth pool.
    let capacity = distinct_fold_slots(
        &params.main_feed_instruments,
        &params.depth_20_instruments,
        &params.depth_200_instruments,
    );
    // Say it at BOOT, before a socket opens, if the universe cannot fit the
    // fold. The aggregator's own refusal is correct but arrives per-tick and
    // is coalesced to one `error!` per PROCESS, so at scale the operator sees
    // a single line and no count — long after the sockets came up looking
    // healthy. This is the same fact, stated once, in advance, with the
    // overflow named.
    if capacity > AGGREGATOR_MAX_SLOTS {
        error!(
            code = ErrorCode::WsGapSubscriptionBatching.code_str(),
            distinct_instruments = capacity,
            slot_ceiling = AGGREGATOR_MAX_SLOTS,
            overflow = capacity.saturating_sub(AGGREGATOR_MAX_SLOTS),
            "the live universe has MORE distinct instruments than the fold has slots — \
             {} instrument(s) will have their ticks refused a slot and produce NO \
             candles, silently from the socket's point of view. The sockets will still \
             open and report healthy. Reduce the universe or raise AGGREGATOR_MAX_SLOTS \
             deliberately (it also sizes the seal ring, the indicator engine and the \
             day-OHLC tracker)",
            capacity.saturating_sub(AGGREGATOR_MAX_SLOTS)
        );
    }
    // INLINE DEPTH ON (operator, 2026-08-19: "i need all these" — all THREE
    // depth sources kept, not a swap).
    //
    // Full mode already delivers 5 levels of bid/ask in every tick packet and
    // this lane already pays 3.24x Quote-mode bandwidth to receive them. Until
    // now the drain read the price and discarded the book. Enabling this keeps
    // the third source alongside the dedicated 20-level and 200-level pools.
    //
    // Honest cost, from measured rows: ~8 GB/day at today's 4,565 instruments,
    // ~44 GB/day at the 25,000 target. Alongside the dedicated pools that is
    // ~126 GB/day of depth at target — which fits the 200 GB volume ONLY
    // because depth retention is current-day (`depth_hot_days = 1`) and every
    // partition is verified into S3 before it leaves local disk. Widen that
    // window and this does not fit.
    let mut ingest = LiveIngest::new(
        TickWriter::new(&params.questdb, Feed::Dhan),
        capacity.max(1),
    )
    // The fold keeps the boot-time pre-size above; the DETECTOR gets the
    // authorized ceiling, because its capacity is a hard cap and the universe
    // grows ~26x after boot when contracts attach. See
    // `with_detector_capacity` for the 1.2M refusals this fixes.
    .with_detector_capacity(AGGREGATOR_MAX_SLOTS)
    .with_inline_depth(DepthIngest::new(&params.questdb));

    // Move the blocking ILP round trip off the drain task, BEFORE any socket
    // opens. See `LiveIngest::spawn_offload_writer` for why a flush on the
    // drain is not merely slow but a tick-loss mechanism.
    //
    // A spawn failure is NOT fatal and NOT silent: the lane falls back to the
    // synchronous path it has always used, which is degraded rather than
    // broken, and says so with a coded error. Refusing to boot over it would
    // trade a real feed for a better-shaped one.
    match ingest.spawn_offload_writer(Arc::clone(&params.feed_health)) {
        Ok(()) => {
            info!(
                "tick writer: the ILP flush now runs on its own thread — a slow \
                 database can no longer stall the frame drain"
            );
        }
        Err(err) => {
            error!(
                code = ErrorCode::HotPath02WriterQueueDrop.code_str(),
                error = %err,
                "tick writer thread could not be spawned — the ILP flush stays ON \
                 the frame drain, where a slow database stalls the fold and ticks \
                 are lost upstream at the vendor. The lane still runs."
            );
        }
    }

    // Seed BEFORE any socket opens, so an instrument that never delivers a
    // single tick is reported as silent rather than being invisible to the gap
    // detector — the difference between "we saw nothing" and "nothing came".
    let seed_millis =
        u64::try_from(chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0).max(0) / 1_000_000)
            .unwrap_or(0);
    let mut seeded = 0usize;
    for instrument in &params.main_feed_instruments {
        if ingest.seed(instrument.security_id, instrument.segment, seed_millis) {
            seeded = seeded.saturating_add(1);
        }
    }

    // ---- re-fold WAL frames recovered from a previous session --------------
    //
    // Deliberately placed AFTER seeding and BEFORE any socket opens. A
    // recovered frame therefore cannot race a live frame for the same
    // `capture_seq`, and the gap detector already knows every instrument, so a
    // recovered tick lands against a seeded slot rather than creating one.
    if !params.wal_replay_live_feed.is_empty() {
        let outcome = refold_wal_frames(&mut ingest, &params.wal_replay_live_feed);
        if outcome.lost == 0 {
            info!(
                frames = params.wal_replay_live_feed.len(),
                ticks = outcome.refolded,
                "recovered live-feed frames from the write-ahead log and folded them — \
                 ticks captured by a previous session are now in the database"
            );
        } else {
            // Never silently green: a frame we could not re-fold is data we
            // captured and then failed to save, which is exactly what this
            // path exists to stop.
            error!(
                code = ErrorCode::WsGapConnectionState.code_str(),
                frames = params.wal_replay_live_feed.len(),
                ticks = outcome.refolded,
                lost = outcome.lost,
                "recovered live-feed frames from the write-ahead log, but {} tick(s) could \
                 NOT be folded — the raw frames remain in the WAL archive and can be \
                 recovered manually",
                outcome.lost
            );
        }

        // CRASH-SAFETY CONFIRM — deliberately HERE, not at boot (2026-08-21).
        //
        // `confirm_replayed` moves the staged segments out of `replaying/` so
        // they never re-stage. Its own doc says to call it "ONLY after the
        // frames returned by `replay_all` have been durably re-captured into
        // the live pipeline". Boot called it unconditionally, several thousand
        // lines BEFORE this refold — the only code that makes that sentence
        // true. Between those two points the segments sat in `archive/`,
        // where the next boot does not look, while the frames they contained
        // were still only in memory. A crash in that window lost them for
        // good, and the archive pruner then deleted the raw bytes on a timer
        // under a doc-comment asserting they were already persisted.
        //
        // Confirming here closes that window. If this process dies before
        // reaching this line, the segments stay in `replaying/` and the next
        // boot replays them again — which is idempotent, because every
        // affected table dedups on its upsert key.
        //
        // Confirmed even when `outcome.lost > 0`: the frames we COULD fold are
        // in the database, and the ones we could not are unfoldable rather
        // than unread — re-replaying them next boot would fail identically
        // while re-staging forever (the WS-REINJECT-01 growth-storm class).
        // The `error!` above is what carries those, and it names the count.
        // Resolved the same way boot resolves it (`TV_WS_WAL_DIR`, else the
        // default) rather than threaded through the params struct: one shared
        // helper cannot drift out of sync with itself, whereas a second copy
        // of the path in a struct field can.
        tickvault_storage::ws_frame_spill::confirm_replayed(crate::boot_helpers::ws_wal_dir());
    }

    let (frame_tx, frame_rx) = tokio::sync::mpsc::channel::<CapturedFrame>(FRAME_RING_CAPACITY);
    // TWO properties, merged rather than picked between (2026-08-15).
    //
    // origin/main SPLIT the ring budget by endpoint so a 512 KiB depth frame
    // can no longer evict main-feed frames. This branch made the budget
    // HOST-DERIVED so a 32 GiB box stops running a ring sized for a 4 GiB one.
    // Taking either alone loses the other: main's split is expressed as
    // fractions of the FRAME_RING_MAX_BYTES constant, so keeping it verbatim
    // would re-introduce the exact hardcoded-for-the-wrong-machine defect this
    // branch removed.
    //
    // The fractions are what compose: apply main's 3/4 : 1/4 split to the
    // derived TOTAL. The host ceiling stays whatever the host can afford, and
    // depth still cannot starve the main feed.
    let ring_max_bytes = frame_ring_max_bytes_for_host();
    info!(
        ring_max_bytes,
        floor_bytes = FRAME_RING_MAX_BYTES,
        auto_sized = ring_max_bytes > FRAME_RING_MAX_BYTES,
        "frame ring byte ceiling selected for this host"
    );
    // Published as GAUGES, not left in a boot log. An auto-sized buffer nobody
    // can see is worse than a fixed one: with a constant you can at least read
    // the source and know the value.
    metrics::gauge!("tv_dhan_feed_ring_max_bytes").set(ring_max_bytes as f64);
    metrics::gauge!("tv_host_total_ram_bytes").set(host_total_ram_bytes().unwrap_or(0) as f64);
    let main_feed_share = ring_max_bytes / 4 * 3;
    let main_feed_budget = Arc::new(RingByteBudget::new(main_feed_share));
    let depth_budget = Arc::new(RingByteBudget::new(ring_max_bytes - main_feed_share));
    // ALWAYS built, and that is load-bearing rather than lazy.
    //
    // The obvious shape — build it only when the boot-time depth sets are
    // non-empty — is WRONG here, and wrong in a way that produces no error at
    // all. Depth instruments do not exist at boot: `main.rs` passes empty
    // vectors BY DESIGN, because a depth contract's `security_id` comes from
    // the option-chain leg, which has not published today's contracts until
    // after 09:16. `attach_depth_when_available` dials the depth sockets
    // LATER, against instruments this function never saw.
    //
    // So a boot-time conditional leaves the writer `None` for the whole
    // session while the late-attached sockets happily deliver frames — every
    // one landing in the `None` arm, counted `depth_unconsumed` and DISCARDED.
    // Sockets connected, frames arriving, nothing stored, no error anywhere:
    // precisely the captured-then-thrown-away behaviour this change set out to
    // end, reintroduced by the guard that was meant to be tidy. It shipped in
    // the first draft of this commit's parent and is fixed here.
    //
    // Cost of building unconditionally: one lazily-connected ILP sender that
    // stays idle if depth never attaches. In exchange `depth_unconsumed` keeps
    // its meaning as a pure wiring-bug signal and should now be unreachable.
    let depth_ingest = Some(DepthIngest::new(&params.questdb));
    // Bounded, and small on purpose: the late-attach pass sends at most a
    // handful of batches in a session. A large buffer here would only delay
    // discovering that nobody is receiving.
    let (seed_tx, seed_rx) = tokio::sync::mpsc::channel::<Vec<SubscribeInstrument>>(8);
    let drain = tokio::spawn(run_frame_drain(
        frame_rx,
        ingest,
        depth_ingest,
        Arc::clone(&main_feed_budget),
        Arc::clone(&depth_budget),
        Arc::clone(&params.shutdown),
        Arc::clone(&params.feed_health),
        seed_rx,
    ));

    // ---- socket lifecycle audit -------------------------------------------
    //
    // ONE consumer for all fifteen market-data sockets, spawned before the
    // first dial so no transition can arrive before a receiver exists. Built
    // unconditionally for the same reason `depth_ingest` above is: the depth
    // sockets attach LATER, and a boot-time conditional would leave them
    // unaudited for the whole session while looking deliberate.
    let ws_audit_tx =
        crate::ws_audit_consumer::spawn_live_feed_lifecycle_audit(params.questdb.clone());

    // ---- the sockets -------------------------------------------------------
    //
    // MAIN-FEED-DIAL-SITE: a deliberate, greppable anchor. Two source-order
    // guards below depend on finding THIS call and not the depth one or the
    // function's own definition, and they used to anchor on the literal
    // `dial_planned_connections(plan` — which broke the moment the argument
    // list wrapped onto its own line for an unrelated reason. A marker that
    // exists to be found cannot be broken by rustfmt; an incidental text
    // pattern can, and when it does the guard fails for a reason that has
    // nothing to do with the invariant it protects.
    // Collects the boot-dialed MAIN-FEED connections' top-up handles. The spot
    // universe packs onto one connection and leaves the rest of it empty; this
    // is how the later contract attach reaches that room instead of stranding
    // it. See `SubscribeGuard::try_extend`.
    let mut main_feed_topups: Vec<(tokio::sync::mpsc::Sender<LiveSubscriptionCommand>, usize)> =
        Vec::new();
    let dialed = dial_planned_connections(
        plan,
        DialContext {
            pool: &mut pool,
            client_id: &client_id,
            spill: &spill,
            frame_tx: &frame_tx,
            main_feed_budget: &main_feed_budget,
            depth_budget: &depth_budget,
            ws_audit_tx: Some(&ws_audit_tx),
            out_topups: Some(&mut main_feed_topups),
            out_depth_commands: None,
        },
    );

    // The connection with the MOST room is the one worth topping up. With a
    // single spot connection that is trivially the only one; picking by
    // `max_by_key` rather than `first` keeps it correct if the spot universe
    // ever spreads, and refuses a connection with zero room rather than
    // handing the attach a channel that can only reject.
    let spot_topup = main_feed_topups
        .into_iter()
        .filter(|(_, spare)| *spare > 0)
        .max_by_key(|(_, spare)| *spare);

    // Depth late-attach. Depth's instrument set is derived from
    // `option_chain_1m`, which the option-chain leg does not populate until its
    // first fire at 09:16 IST — but this stack is spawned at boot (~08:30). A
    // boot-time load therefore asks for the set ~45 minutes before it exists,
    // which is why depth has opened ZERO of its ten authorized sockets every
    // session, and why the empty-selection log prescribed a manual restart.
    //
    // A WEAK sender, never a clone: a held `Sender` would keep the ring open
    // for the whole wait, so if every main-feed socket died in that window the
    // drain could not close and the lane would read alive while producing
    // nothing — the same false-OK, arriving from a new direction. `upgrade()`
    // additionally gives depth a CORRECT answer when the lane died while it
    // waited: it declines to dial into a dead ring rather than opening sockets
    // that feed nothing.
    //
    // Spawned only when the operator supplied no depth set at boot; an
    // explicit set is already dialed above and must not be second-guessed.
    if params.depth_20_instruments.is_empty() && params.depth_200_instruments.is_empty() {
        tokio::spawn(attach_depth_when_available(
            pool,
            params.questdb.clone(),
            client_id.clone(),
            Arc::clone(&spill),
            frame_tx.downgrade(),
            Arc::clone(&depth_budget),
            Arc::clone(&main_feed_budget),
            main_feed_connections_for(
                params.main_feed_instruments.len(),
                usize::from(tickvault_core::websocket::pool_budget::MAX_MAIN_FEED_CONNECTIONS),
            ),
            spot_topup,
            ws_audit_tx.clone(),
            seed_tx.clone(),
        ));
    }

    // Drop the template sender: while it lived, the ring could never close, so
    // the drain would hang forever after the last socket died instead of
    // reporting that the lane went dark. The depth attach holds only a WEAK
    // handle, so this drop stays exactly where it has always been.
    drop(frame_tx);

    if dialed == 0 {
        warn!(
            code = ErrorCode::WsGapConnectionState.code_str(),
            "Dhan live feed planned zero connections — nothing was dialed and no live market \
             data will flow this session"
        );
        return;
    }

    // CORRECTED 2026-08-20 (plan Item 7, which had never been implemented).
    //
    // This line used to `.set(1.0)` HERE, and the comment above it claimed the
    // gauge meant "sockets dialed AND a fold consuming them". It did not. The
    // `dialed` counter it sits behind is incremented immediately after
    // `tokio::spawn`, and the connect happens INSIDE that task — so a socket
    // that answers HTTP 400 forever still counts as dialed.
    //
    // That is not hypothetical. On 2026-08-12 the main feed failed twelve
    // dials with `400 Bad Request`, produced ZERO candles for a 373-minute
    // session, and this gauge would have read 1.0 for every minute of it —
    // leaving `tv-<env>-dhan-live-lane-down` silent through precisely the
    // outage it exists to catch. A monitor that cannot fire during its own
    // scenario is worse than no monitor: it converts an outage into a clean
    // dashboard.
    //
    // The gauge is now raised by the DRAIN, on the first frame it actually
    // receives — the earliest moment at which "sockets dialed AND a fold
    // consuming them" is a true statement rather than an intention. Here we
    // only pre-register it at zero, which is the house first-sample-baseline
    // discipline: the CloudWatch agent drops the first delta sample, and the
    // sample it drops must be the harmless 0 rather than the session's first
    // real transition.
    metrics::gauge!(FEED_STACK_UP_GAUGE).set(0.0);

    // Tell the operator-facing feed state the same thing the gauge just said.
    //
    // Before 2026-08-14 `set_dhan_lane_running` had ZERO production call sites
    // — eight matches repo-wide, every one of them in a test. The flag is
    // initialised `false` and nothing ever moved it, so `feed_health` reported
    // `Degraded: "enabled, but the feed was not started at boot"` for a lane
    // that was healthy, a lane that was dead, and a lane that had never been
    // configured, identically and forever. The operator console printed that
    // constant as if it were a diagnosis, and the feeds page then prescribed a
    // restart from it. A status line that cannot vary is not a status line.
    params.feed_runtime.set_dhan_lane_running(true);
    info!(
        dialed,
        seeded,
        ring_capacity = FRAME_RING_CAPACITY,
        // BOTH bounds, because reporting only the frame count is what let a
        // 16 GiB memory ceiling hide behind a 65,536 that looks modest. An
        // operator reading this line should be able to see the queue's real
        // size in the unit that runs out.
        ring_max_bytes = main_feed_budget.cap() + depth_budget.cap(),
        // Both shares, because the split is the thing an operator needs to
        // reason about when one endpoint starts refusing and the other does not.
        ring_main_feed_max_bytes = main_feed_budget.cap(),
        ring_depth_max_bytes = depth_budget.cap(),
        "Dhan 16-connection live feed is up: sockets dialed, frames captured to the WAL before \
         broadcast, and the tick fold is consuming the ring"
    );

    // Hold the task alive with the drain so the stack's JoinHandle reflects the
    // lane's real lifetime rather than completing the instant it finished
    // dialing.
    let drain_outcome = drain.await;

    // Clear the up-gauge on EVERY exit, not just the error one.
    //
    // Until 2026-08-14 this was inside the `if let Err(...)` arm below, which
    // left a false-OK with a very specific shape: when the drain returns
    // NORMALLY — which is what happens once every socket has died and the ring
    // closes — `drain.await` is `Ok`, the arm never runs, and
    // `tv_dhan_feed_stack_up` stays pinned at 1.0 for the rest of the process.
    // The one metric whose entire job is to say "the lane is carrying data"
    // would report a healthy lane precisely when every socket was gone. The
    // gauge now falls on the normal path, the error path, and the
    // nothing-dialed path alike.
    metrics::gauge!(FEED_STACK_UP_GAUGE).set(0.0);
    params.feed_runtime.set_dhan_lane_running(false);

    if let Err(err) = drain_outcome {
        // The drain is the ONLY consumer of the ring. If it panicked, every
        // socket is still capturing to the WAL but nothing is folding — the
        // exact shape of a lane that looks alive and produces no candles.
        //
        // Honest note on reachability (verified 2026-08-14): the release
        // profile sets `panic = "abort"`, so a genuine panic in the drain
        // aborts the PROCESS rather than surfacing here, and systemd
        // (`Restart=always`, `RestartSec=3`) brings the lane back within
        // seconds. In a release build this arm is therefore reached only by
        // task cancellation, not by a panic. It is retained because it IS
        // reachable in a debug build and because a silent join failure must
        // never pass unlogged — but nobody should read it as the production
        // recovery story. The production recovery story is the process
        // restart, and it already works.
        error!(
            code = ErrorCode::WsGapConnectionState.code_str(),
            %err,
            "the Dhan live-feed frame drain DIED — frames are still being captured to the \
             write-ahead log but nothing is folding them into candles this session"
        );
    } else {
        info!(
            "the Dhan live-feed frame drain exited cleanly — every socket is closed and the \
             ring is drained; the lane is reporting itself DOWN"
        );
    }
}

// ---------------------------------------------------------------------------
// 15:31 cross-verification — the lane's only ground truth
// ---------------------------------------------------------------------------

/// Counter: daily cross-verify runs that could not start because no dependency
/// provider was installed.
pub const XVERIFY_UNPROVISIONED_COUNTER: &str = "tv_dhan_feed_xverify_unprovisioned_total";

/// Everything the 15:31 comparator needs that this module cannot derive on its
/// own. Registered once at boot via [`install_crossverify_deps`].
///
/// This is a registration seam rather than a field on
/// [`DhanFeedStackParams`] deliberately: the params struct is built by
/// `main.rs` with an exhaustive struct literal, so adding a required field
/// there would break a file this module does not own. A provider that is never
/// installed degrades loudly (see [`spawn_daily_crossverify`]) instead of
/// silently skipping the verification.
pub struct CrossverifyDeps {
    /// QuestDB `/exec` endpoint the live side is read from.
    pub questdb_exec_url: String,
    /// Dhan intraday-candles endpoint the REST side is fetched from.
    pub intraday_url: String,
    /// Returns a currently-valid Dhan JWT, or `None` when the token manager
    /// has none. A closure rather than a value because the token rotates every
    /// ~23h and this scheduler outlives any single token.
    pub jwt_provider: Box<dyn Fn() -> Option<String> + Send + Sync>,
    /// Comparator knobs.
    pub config: crate::dhan_live_crossverify::DhanLiveCrossverifyConfig,
    /// QuestDB connection used to PERSIST the run's findings.
    ///
    /// Separate from `questdb_exec_url` above because that one is the HTTP
    /// `/exec` READ endpoint and this is the ILP WRITE config — the same
    /// server, two protocols. Added 2026-08-25 with the persistence wiring:
    /// before it, this comparator produced its verdict, logged it, and threw
    /// it away.
    pub questdb: tickvault_common::config::QuestDbConfig,
}

static CROSSVERIFY_DEPS: std::sync::OnceLock<CrossverifyDeps> = std::sync::OnceLock::new();

/// Installs the cross-verify dependencies. Idempotent: the first call wins and
/// later calls return `false` rather than replacing a live provider.
pub fn install_crossverify_deps(deps: CrossverifyDeps) -> bool {
    CROSSVERIFY_DEPS.set(deps).is_ok()
}

/// Whether a provider has been installed.
#[must_use]
pub fn crossverify_deps_installed() -> bool {
    CROSSVERIFY_DEPS.get().is_some()
}

/// Dhan's `instrument` string for a segment, or `None` when the segment alone
/// cannot determine it.
///
/// Added 2026-08-25. `crossverify_targets` used to stamp `"INDEX"` on EVERY
/// target, and that string goes verbatim into the Dhan REST intraday body. The
/// live universe is ~119 indices plus ~750 NSE_EQ constituents, so roughly 86%
/// of every run's fetches asked for a STOCK as though it were an INDEX. Those
/// return no candles, land in the `rest_failures` bucket, and are never
/// compared — while the run can still report `Clean` on the handful of real
/// indices that happened to be labelled correctly.
///
/// That is a PARTIAL-denominator vacuous pass, and it is invisible to the
/// module's `minutes_compared > 0` guard, which only catches a ZERO
/// denominator. The comparator's own doc comment says it "can never verify a
/// different universe than it captured" — true of the id set, false of the
/// instrument type, and the type is what decides whether a fetch returns
/// anything at all.
///
/// F&O returns `None` deliberately. `(security_id, segment)` is all the
/// subscribe set carries, and `NSE_FNO` could be `FUTIDX`, `OPTIDX`, `FUTSTK`
/// or `OPTSTK` — a guess would land back in the silent-failure bucket this
/// exists to empty. An unverifiable target is counted and named, not fetched
/// with a wrong label.
#[must_use]
pub fn dhan_intraday_instrument_for(segment: ExchangeSegment) -> Option<&'static str> {
    match segment {
        ExchangeSegment::IdxI => Some("INDEX"),
        ExchangeSegment::NseEquity | ExchangeSegment::BseEquity => Some("EQUITY"),
        // Ambiguous from the segment alone; see the doc above.
        ExchangeSegment::NseFno | ExchangeSegment::BseFno => None,
        // Out of the authorized scope entirely.
        ExchangeSegment::NseCurrency | ExchangeSegment::BseCurrency | ExchangeSegment::McxComm => {
            None
        }
    }
}

/// Builds the comparator's target list from the subscribed main-feed set, so
/// the lane can never verify a different universe than it captured.
///
/// Returns the targets plus the count of subscribed instruments that CANNOT be
/// targeted, because a wrong `instrument` label is worse than an absent one: it
/// fetches nothing while looking like a fetch that failed.
#[must_use]
pub fn crossverify_targets_with_skipped(
    main_feed: &[SubscribeInstrument],
) -> (Vec<crate::dhan_live_crossverify::XverifyTarget>, usize) {
    let mut targets = Vec::with_capacity(main_feed.len());
    let mut skipped = 0_usize;
    for i in main_feed {
        let (Some(instrument), Ok(security_id)) = (
            dhan_intraday_instrument_for(i.segment),
            i64::try_from(i.security_id),
        ) else {
            // 2026-08-25: the id arm used to be `unwrap_or(0)`, which turned an
            // out-of-range id into a target for instrument 0 — the comparator
            // would then verify, and report on, an instrument that does not
            // exist.
            skipped = skipped.saturating_add(1);
            continue;
        };
        targets.push(crate::dhan_live_crossverify::XverifyTarget {
            security_id,
            segment: i.segment.as_str().to_string(),
            instrument: instrument.to_string(),
        });
    }
    (targets, skipped)
}

/// Convenience wrapper for callers that only need the targets.
#[must_use]
pub fn crossverify_targets(
    main_feed: &[SubscribeInstrument],
) -> Vec<crate::dhan_live_crossverify::XverifyTarget> {
    crossverify_targets_with_skipped(main_feed).0
}

/// Spawns the daily comparator (see [`XVERIFY_RUN_AT_SECS_OF_DAY_IST`]) for the
/// subscribed universe.
///
/// Returns `None` — loudly — when no [`CrossverifyDeps`] were installed. That
/// is a refusal, not a skip: a live lane with no verifier has no way to detect
/// the packet loss its protocol cannot report, and saying so is the whole
/// point of audit Rule 11.
pub fn spawn_daily_crossverify(
    main_feed: &[SubscribeInstrument],
) -> Option<tokio::task::JoinHandle<()>> {
    let (targets, skipped) = crossverify_targets_with_skipped(main_feed);
    if skipped > 0 {
        // Named, never silent. These instruments are captured by the lane and
        // CANNOT be verified against the vendor tape, which is a coverage hole
        // in the lane's only ground truth — the operator must be able to see
        // its size rather than infer it from a `rest_failures` count that also
        // carries genuine failures.
        metrics::counter!("tv_dhan_xverify_targets_unverifiable_total").increment(skipped as u64);
        warn!(
            skipped,
            targeted = targets.len(),
            "cross-verification cannot target every subscribed instrument: an F&O \
             contract's Dhan `instrument` string (FUTIDX / OPTIDX / FUTSTK / OPTSTK) \
             is not derivable from its segment alone, and a wrong label fetches \
             nothing while looking like a failed fetch. These instruments are \
             CAPTURED but UNVERIFIED."
        );
    }
    if !crossverify_deps_installed() {
        metrics::counter!(XVERIFY_UNPROVISIONED_COUNTER).increment(1);
        error!(
            code = ErrorCode::WsGapConnectionState.code_str(),
            targets = targets.len(),
            "Dhan live feed is enabled but the 15:31 cross-verification has NO dependency \
             provider installed, so it cannot run. The main feed has no snapshot-on-subscribe \
             and no sequence number: without this comparator, packet loss is UNDETECTABLE. \
             Call install_crossverify_deps() at boot before enabling the lane."
        );
        return None;
    }
    Some(tokio::spawn(async move {
        // 2026-08-11 — this body used to be a single `info!` saying the
        // verification was "scheduled". Nothing was scheduled. The lane's ONLY
        // loss detector was a log line, and the log line said it was working:
        // the precise false-OK shape audit Rule 11 exists to forbid, in the
        // one place where being wrong is undetectable by any other means (the
        // main feed carries no sequence number and no snapshot-on-subscribe).
        info!(
            targets = targets.len(),
            run_at_ist = %run_at_ist_hhmm(),
            "Dhan live-feed cross-verification armed — it will compare captured candles \
             against Dhan's own REST record after the close"
        );
        loop {
            let sleep_secs = secs_until_next_run_ist(now_ist_secs_of_day());
            tokio::time::sleep(std::time::Duration::from_secs(sleep_secs)).await;

            // Same trading-day gate as the silence detector, for the same
            // reason. On a weekday NSE holiday both sides of this comparison
            // are legitimately empty, and the run reports "found no data on
            // either side today" — a warning about the lane's ONLY loss
            // detector, fired on a day it had nothing to detect. Left ungated
            // it compounds the silence detector's false page into a pattern the
            // operator learns to ignore.
            if !silence_page_allowed_today() {
                info!(
                    "Dhan live-feed cross-verification skipped — not an NSE trading day. \
                     No candles were expected, so there is nothing to verify."
                );
                continue;
            }

            let Some(deps) = CROSSVERIFY_DEPS.get() else {
                // Unreachable in practice (the caller checked), but a `let
                // else` beats an unwrap on a path that must never panic a
                // long-lived task.
                return;
            };
            let Some(jwt) = (deps.jwt_provider)() else {
                counters().xverify_no_token.increment(1);
                error!(
                    code = ErrorCode::WsGapConnectionState.code_str(),
                    "Dhan live-feed cross-verification could not run: no JWT available. The \
                     day's captured candles are UNVERIFIED — packet loss for this session is \
                     undetectable."
                );
                continue;
            };

            let ist = tickvault_common::trading_calendar::ist_offset();
            let today = chrono::Utc::now().with_timezone(&ist).date_naive();
            // IST-wall-clock-as-epoch, NOT the true UTC instant of IST
            // midnight. `and_utc()`, deliberately, on a date that is already
            // the IST date.
            //
            // FIXED 2026-08-11, and this was blind-since-birth. The previous
            // line was `.and_local_timezone(ist)`, which yields the real UTC
            // instant — 18:30Z the previous day. But BOTH sides of this
            // comparison stamp IST wall-clock as though it were epoch: the
            // live side because `ticks.ts` is `exchange_timestamp * 1e9` with
            // no offset (Dhan's LTT is already IST epoch seconds — see
            // data-integrity.md, "NEVER ADD +5:30 TO ts"), and the REST side
            // because `intraday_utc_secs_to_ist_minute_nanos` adds the offset
            // to a UTC epoch. Subtracting a true-UTC origin from an
            // IST-wall-as-epoch value therefore produced `wall_secs + 19800`.
            //
            // The consequence was not a small drift. `is_in_session` accepts
            // [33300, 55800); with the skew, a bucket left the window as soon
            // as `wall_secs >= 36000` — 10:00 IST. Every minute from 10:00
            // onward was dropped as `out_of_session` on BOTH sides before the
            // join, so the comparison saw 45 of the day's 375 session minutes
            // and, because `out_of_session` feeds no verdict and 45 is not
            // vacuous, still reported Clean. The tail amnesty landed on
            // 09:58-09:59 instead of 15:28-15:29, hiding the genuine tail too.
            //
            // This is the SAME defect class as the nanosecond-vs-microsecond
            // bug that made the 2026-07 cross-verify blind since birth and
            // helped retire the feed — re-created in a different coordinate
            // system, in the one check that exists to catch disagreement.
            let day_start_ist_nanos = today
                .and_hms_opt(0, 0, 0)
                .and_then(|dt| dt.and_utc().timestamp_nanos_opt())
                .unwrap_or(0);
            debug_assert_eq!(
                day_start_ist_nanos % (24 * 3600 * 1_000_000_000_i64),
                0,
                "an IST-wall-as-epoch midnight must land exactly on a day boundary; a \
                 non-zero remainder means a real-timezone origin crept back in"
            );

            let client = reqwest::Client::new();
            match crate::dhan_live_crossverify::run_cross_verification(
                &client,
                &deps.questdb_exec_url,
                &deps.intraday_url,
                &jwt,
                &targets,
                today,
                day_start_ist_nanos,
                &deps.config,
            )
            .await
            {
                Ok(report) => {
                    let c = &report.comparison;
                    // Split BEFORE the log line, so the counter and the
                    // `vacuous = ` field below can never disagree.
                    if c.is_vacuous() {
                        counters().xverify_vacuous.increment(1);
                    } else {
                        counters().xverify_measured.increment(1);
                    }
                    // THE VERDICT AS FIELDS, not as a debug dump.
                    //
                    // 2026-08-20, measured on the box: this emitted `?report`,
                    // which renders every finding. Today's run produced a
                    // 1,048,374-character line — EXACTLY CloudWatch's 1 MiB
                    // event ceiling, so it was truncated. `RunReport`'s Debug
                    // puts `findings` before the totals, which means the
                    // truncation ate precisely the summary: `minutes_compared`
                    // — the non-vacuity denominator this whole job exists to
                    // produce — was unreadable, while thousands of individual
                    // findings were not.
                    //
                    // The single most important measurement in the system was
                    // the one number the log could not carry. Named fields are
                    // bounded by construction and queryable; the per-cell
                    // detail belongs in the audit table, which is what the
                    // `persist_xverify_report` call below writes.
                    //
                    // ⚠ CORRECTED 2026-08-25. This comment previously read
                    // "the findings are already persisted to the audit table"
                    // — and that was FALSE. `append_cell`, `append_daily` and
                    // even `ensure_dhan_live_crossverify_tables` had ZERO
                    // production callers: the two tables were never created,
                    // nothing was ever written, and the only record of the
                    // feed's one ground-truth check was this log line. A
                    // comment asserting persistence is worse than no comment,
                    // because the next reader stops looking.
                    info!(
                        targets = targets.len(),
                        outcome = ?c.outcome,
                        instruments = c.instruments,
                        minutes_compared = c.minutes_compared,
                        cells_diverged = c.cells_diverged,
                        missing_live = c.missing_live,
                        // The split that makes `missing_live` actionable.
                        // At 31.2% of fetched minutes on 2026-08-25 the
                        // single figure could mean a catastrophe or a
                        // non-event; these two say which. See
                        // `DayComparison::missing_live_traded` — and note
                        // the pair is uninformative for IDX_I, which has no
                        // volume at all.
                        missing_live_traded = c.missing_live_traded,
                        missing_live_zero_volume = c.missing_live_zero_volume,
                        missing_rest = c.missing_rest,
                        tail_unsealed = c.tail_unsealed,
                        out_of_session = c.out_of_session,
                        noise_p50_paise = c.noise_p50_paise,
                        noise_p95_paise = c.noise_p95_paise,
                        noise_max_paise = c.noise_max_paise,
                        // The match RATE, computed rather than left as an
                        // exercise. Both inputs were already on this line,
                        // so anyone could multiply by four and subtract —
                        // and nobody did, for two sessions, while the run
                        // sat at 0.09% of its intended coverage. A number
                        // that needs arithmetic before it means anything is
                        // a number that gets skipped.
                        price_fields_compared = c.minutes_compared.saturating_mul(4),
                        price_fields_agreed = c
                            .minutes_compared
                            .saturating_mul(4)
                            .saturating_sub(c.cells_diverged),
                        // Volume — reported for the first time today. A
                        // capture percentage, never a pass/fail: see the
                        // volume block on `DayComparison`.
                        volume_cells = c.volume_cells,
                        volume_exact = c.volume_exact,
                        volume_capture_p50_pct = c.volume_capture_p50_pct,
                        volume_capture_p05_pct = c.volume_capture_p05_pct,
                        volume_capture_min_pct = c.volume_capture_min_pct,
                        findings = c.findings.len(),
                        rest_failures = report.rest_failures,
                        // Added 2026-08-26. `rest_failures` alone reported
                        // 814-of-864 and 815-of-865 on consecutive sessions
                        // and gave nobody a way to act on it: the reason was
                        // discarded at the fetch site. This field names the
                        // dominant cause on the same line as the verdict.
                        rest_failure_reasons = %report.rest_failure_breakdown.summary(),
                        malformed_rows = report.malformed_rows,
                        budget_elapsed = report.budget_elapsed,
                        // Reported beside `degraded`, never folded into it: a
                        // single REST failure also sets `degraded`, so the two
                        // together were indistinguishable — which is why the
                        // 2026-08-26 short live read left no trace in this
                        // line even though the run's own counts summed to the
                        // cap exactly.
                        live_truncated = report.live_truncated,
                        degraded = report.degraded,
                        vacuous = c.is_vacuous(),
                        "Dhan live-feed cross-verification finished — this is the honest \
                         measure of whether the revived feed agrees with Dhan's own record"
                    );
                    // PERSIST, before any early-return branch below. A
                    // vacuous or degraded verdict is exactly the one worth
                    // keeping: "we could not measure today" is a fact about
                    // the feed, and a table that only records the good days
                    // cannot answer "how often were we blind last month".
                    persist_xverify_report(&deps.questdb, &report, deps.config.tolerance_paise);
                    if c.is_vacuous() {
                        // A run that compared nothing proves nothing, and the
                        // outcome field alone does not say so loudly enough.
                        //
                        // `source` is what makes this line REACHABLE by an
                        // alarm. `WS-GAP-03` has 25 emit sites in this file
                        // alone — ordinary dial failures, reconnects and pool
                        // supervisor events all carry it — so a filter keyed
                        // on the code alone would page on connection churn,
                        // which is the noise trap
                        // `dhan-rest-only-noise-lock-2026-07-14.md` §2.3d-i
                        // records (a bare-code filter was proposed there,
                        // approved, and then found wrong for exactly this
                        // reason). The shape that works is the three-condition
                        // one that section settled on:
                        // `{ $.code = "WS-GAP-03" && $.level = "ERROR" &&
                        //    $.source = "xverify_vacuous" }` — and it cannot
                        // be written at all until the field exists here.
                        //
                        // Adding the field is NOT adding a page: nothing
                        // filters on it yet. The alarm itself still needs a
                        // dated operator quote per that file's §3, and the
                        // metric route is blocked by ONE BYTE, measured
                        // 2026-08-25: the EMF selector lives in a user-data
                        // template whose render is 15,841 of a 15,872-byte
                        // budget, so 31 bytes are free — and
                        // `tv_dhan_feed_xverify_runs_total` is 31 characters,
                        // which with its separating pipe needs 32. So the
                        // counter is in neither selector copy and never
                        // reaches CloudWatch, and it cannot be added without
                        // first removing something or moving the selector out
                        // of user-data entirely.
                        error!(
                            code = ErrorCode::WsGapConnectionState.code_str(),
                            source = "xverify_vacuous",
                            targets = targets.len(),
                            missing_live = c.missing_live,
                            missing_rest = c.missing_rest,
                            "Dhan live-feed cross-verification compared ZERO minutes — the day's \
                             captured candles are UNVERIFIED. This is not a pass with no findings; \
                             it is no measurement at all"
                        );
                    }
                }
                Err(err) => {
                    counters().xverify_failed.increment(1);
                    error!(
                        code = ErrorCode::WsGapConnectionState.code_str(),
                        source = "xverify_failed",
                        %err,
                        "Dhan live-feed cross-verification FAILED to run — the day's captured \
                         candles are UNVERIFIED, never assume they are clean"
                    );
                }
            }
        }
    }))
}

/// IST seconds-of-day at which the comparator runs: 15:41, one minute after
/// the 15:40 close, so the final minute's candle has sealed.
///
/// **CORRECTED 2026-08-25** from 15:31, in lockstep with
/// `dhan_live_crossverify::SESSION_CLOSE_SECS_OF_DAY_IST`. Both had missed the
/// 2026-08-07 NSE CAS migration that moved the session end 15:30 -> 15:40.
///
/// The two MUST move together, and the const assert below is what enforces it.
/// Moving the window without the fire time would be strictly worse than the
/// drift it fixes: the comparator would run at 15:31 against a window ending at
/// 15:40, so ten minutes that had not happened yet would be scored as missing on
/// BOTH sides — turning a silent blind spot into a flood of false loss findings
/// in the one check that exists to detect real loss.
/// Writes one cross-verification run to its two audit tables.
///
/// # Why this exists, and why it is best-effort
///
/// The 15:41 comparison is the ONLY ground truth the revived Dhan feed has:
/// the India feed carries no sequence number and offers no
/// snapshot-on-subscribe, so packet loss is undetectable at the protocol
/// level. Until 2026-08-25 the result of that comparison was written to a log
/// line and discarded — `append_cell`, `append_daily` and
/// `ensure_dhan_live_crossverify_tables` all had zero production callers, so
/// there was no history, no trend, and no way to ask how often the feed
/// disagreed with Dhan's own record last month.
///
/// Best-effort by construction: a persistence failure logs and returns. This
/// runs once a day on a cold-path task, long after the market has closed, and
/// failing the task would lose the log line too — which is strictly worse than
/// losing the table row, since the log line is what the operator sees today.
///
/// Complexity is O(findings) with one ILP buffer; the row count is bounded by
/// the comparison itself (one row per divergent/missing cell), and the
/// findings vector already exists in memory — this adds no allocation beyond
/// the ILP buffer.
// TEST-EXEMPT: thin ILP-write shell over the fully-tested writer (append_cell / append_daily / flush are unit-tested in tickvault_storage) and a pure row mapping asserted by `the_daily_row_carries_every_comparison_total` below.
fn persist_xverify_report(
    questdb: &tickvault_common::config::QuestDbConfig,
    report: &crate::dhan_live_crossverify::RunReport,
    tolerance_paise: i64,
) {
    use tickvault_storage::dhan_live_crossverify_persistence::DhanLiveXverifyAuditWriter;

    let c = &report.comparison;
    let mut writer = DhanLiveXverifyAuditWriter::new(questdb);

    // ---- flush in batches -------------------------------------------
    //
    // MEASURED, prod 2026-08-26: one run produced 764,003 rows and a
    // 207,965,278-byte buffer against a 104,857,600-byte ceiling. The flush
    // failed and **the entire day's comparison was discarded** — in the one
    // check that exists to prove nothing was lost.
    //
    // The scope filter is the root-cause fix and cuts the row count by ~99%.
    // This is the floor under it: a future day that is legitimately large
    // loses at most one batch instead of everything. Two independent
    // mechanisms, because "the row count can never grow again" is exactly the
    // assumption that produced the ceiling breach.
    //
    // 20,000 rows is deliberate, not round: the breached buffer averaged
    // ~272 bytes/row, so a batch lands near 5 MB — comfortably inside the
    // 100 MB ceiling with two orders of magnitude of headroom for a row
    // shape that grows.
    const PERSIST_BATCH_ROWS: usize = 20_000;
    let mut batch_errors = 0_usize;
    //
    // MERGED 2026-08-27: two sessions fixed this independently, one bounding
    // ROWS and one bounding BYTES, and the bounds are not interchangeable.
    // Rows give a predictable batch size; BYTES is the quantity that actually
    // breached the ceiling, and it is the one that survives a row shape
    // getting wider — which the batch-size note directly above names as the
    // thing it is buying headroom against. Keeping only the row bound would
    // have re-created that assumption one layer down.
    let flush_if_full = |w: &mut DhanLiveXverifyAuditWriter, errs: &mut usize| {
        let failed = if w.pending() >= PERSIST_BATCH_ROWS {
            w.flush().is_err()
        } else {
            // Byte-bounded: a no-op below the threshold, one `len()` compare.
            w.flush_if_large().is_err()
        };
        if failed {
            // One batch lost, named, and the run continues. Before this the
            // same failure took the whole day with it.
            *errs += 1;
        }
    };

    let mut cell_errors = 0_usize;
    for finding in &c.findings {
        if writer.append_cell(finding).is_err() {
            cell_errors += 1;
        }
        flush_if_full(&mut writer, &mut batch_errors);
    }

    // The vendor's own tape, stored BEFORE any judgement is applied to it.
    // Until 2026-08-26 these rows were compared in memory and dropped, so the
    // only surviving trace of what the exchange actually said was the subset
    // that happened to DISAGREE.
    let mut tape_errors = 0_usize;
    for row in &report.rest_tape {
        if writer.append_rest_tape(row).is_err() {
            tape_errors += 1;
        }
        flush_if_full(&mut writer, &mut batch_errors);
    }

    // The daily row now meets a nearly-empty buffer. That matters more than it
    // looks: it is appended AFTER the cells, so under the old single-flush
    // shape an oversized cell buffer destroyed the one row recording that a
    // comparison happened at all — the detail and the evidence of its loss
    // went in the same refusal.
    let daily = xverify_daily_row(c, tolerance_paise);
    let daily_err = writer.append_daily(&daily).err();

    match writer.flush() {
        Ok(()) => {
            metrics::counter!(XVERIFY_PERSIST_ROWS_COUNTER)
                .increment(c.findings.len() as u64 + report.rest_tape.len() as u64 + 1);
            if cell_errors > 0 || daily_err.is_some() || tape_errors > 0 || batch_errors > 0 {
                // Partial writes are reported, never rounded up to success:
                // an audit table that silently drops rows is worse than one
                // that is honestly incomplete.
                //
                // `chunk_flush_errors` joined this condition with the
                // 2026-08-26 chunking, and it is the reason chunking is safe
                // to do at all. Draining in pieces converts "no rows today"
                // into "most rows today", which is an IMPROVEMENT only while
                // the shortfall is visible — a silently-short audit table
                // reads as a complete one. Each failed chunk discards its own
                // buffer, so the rows lost are bounded by the flush threshold
                // rather than by the run.
                error!(
                    code = ErrorCode::WsGapConnectionState.code_str(),
                    source = "xverify_persist_partial",
                    cell_errors,
                    tape_errors,
                    batch_errors,
                    daily_failed = daily_err.is_some(),
                    findings = c.findings.len(),
                    tape_rows = report.rest_tape.len(),
                    "Dhan live-feed cross-verification persisted with gaps — some findings \
                     could not be appended or a chunk flush was refused, so the audit \
                     tables are incomplete for today"
                );
            }
        }
        Err(err) => {
            let discarded = writer.discard_pending();
            metrics::counter!(XVERIFY_PERSIST_ERRORS_COUNTER).increment(1);
            error!(
                code = ErrorCode::WsGapConnectionState.code_str(),
                // Deliberately the SAME label the run-failure arm uses, not a
                // new one. The operator consequence is identical — there is
                // no verdict on record for today — and `xverify_failed` is
                // one of only two xverify labels an alarm matches on. A
                // distinct label would be better triage and would page
                // nobody, which is the trade this repository has got wrong
                // before. The message below is what separates the causes.
                source = "xverify_failed",
                ?err,
                discarded,
                "Dhan live-feed cross-verification could NOT be persisted — today's \
                 comparison exists only in this log stream. The feed's one ground-truth \
                 record has no row for today; check QuestDB before the next session."
            );
        }
    }
}

/// Maps a finished comparison onto its daily audit row. Pure.
///
/// Separated from the write so the mapping is testable without QuestDB — the
/// failure this guards against is a column silently carrying the wrong total,
/// which no integration test would notice and no log line would show.
#[must_use]
fn xverify_daily_row(
    c: &crate::dhan_live_crossverify::DayComparison,
    tolerance_paise: i64,
) -> tickvault_storage::dhan_live_crossverify_persistence::DhanLiveXverifyDailyRow {
    use tickvault_storage::dhan_live_crossverify_persistence::DhanLiveXverifyDailyRow;
    // Every finding carries the run stamp and the trading day the comparison
    // was FOR, so the daily row is stamped from the same source rather than
    // from `now()` — a rerun must UPSERT onto the same row, not append a
    // second one an hour later.
    let (run_ts, day_ts) = c
        .findings
        .first()
        .map_or((0, 0), |f| (f.run_ts_ist_nanos, f.trading_date_ist_nanos));
    DhanLiveXverifyDailyRow {
        run_ts_ist_nanos: run_ts,
        trading_date_ist_nanos: day_ts,
        instruments: c.instruments,
        minutes_compared: c.minutes_compared,
        cells_diverged: c.cells_diverged,
        missing_live: c.missing_live,
        missing_live_traded: c.missing_live_traded,
        missing_live_zero_volume: c.missing_live_zero_volume,
        missing_rest: c.missing_rest,
        tail_unsealed: c.tail_unsealed,
        out_of_session: c.out_of_session,
        noise_p50_paise: c.noise_p50_paise,
        noise_p95_paise: c.noise_p95_paise,
        noise_max_paise: c.noise_max_paise,
        tolerance_paise,
        outcome: c.outcome,
    }
}

/// Rows successfully written to the cross-verification audit tables.
pub const XVERIFY_PERSIST_ROWS_COUNTER: &str = "tv_dhan_feed_xverify_rows_total";
/// Runs whose findings could not be persisted at all.
pub const XVERIFY_PERSIST_ERRORS_COUNTER: &str = "tv_dhan_feed_xverify_persist_errors_total";

pub const XVERIFY_RUN_AT_SECS_OF_DAY_IST: u64 =
    crate::dhan_live_crossverify::RUN_SECS_OF_DAY_IST as u64;

const _: () = assert!(
    XVERIFY_RUN_AT_SECS_OF_DAY_IST as i64
        > crate::dhan_live_crossverify::SESSION_CLOSE_SECS_OF_DAY_IST,
    "the comparator must fire AFTER the last minute of the window it compares"
);

/// The comparator's fire time as a `HH:MM` IST string, DERIVED.
///
/// Added 2026-08-25 because the arming log line carried a hardcoded `"15:31"`
/// that survived the CAS correction above by three constants. A literal in an
/// operator-facing field is the same class of defect as a literal in a
/// comparison window — it just fails quietly, by telling the operator a time
/// the code no longer uses.
#[must_use]
pub fn run_at_ist_hhmm() -> String {
    let h = XVERIFY_RUN_AT_SECS_OF_DAY_IST / 3_600;
    let m = (XVERIFY_RUN_AT_SECS_OF_DAY_IST % 3_600) / 60;
    format!("{h:02}:{m:02}")
}

/// Seconds in a day.
const SECS_PER_DAY: u64 = 24 * 3_600;

/// Seconds to sleep from `now_secs_of_day` until the next run time (see
/// [`XVERIFY_RUN_AT_SECS_OF_DAY_IST`] — 15:41 IST today).
///
/// Pure, so the schedule is testable without waiting a day. Returns a full day
/// when called exactly at the run time, which is the right way round: firing
/// twice in one session would double-count the verdict, and firing a day late
/// merely delays it.
#[must_use]
pub const fn secs_until_next_run_ist(now_secs_of_day: u64) -> u64 {
    if now_secs_of_day < XVERIFY_RUN_AT_SECS_OF_DAY_IST {
        XVERIFY_RUN_AT_SECS_OF_DAY_IST - now_secs_of_day
    } else {
        SECS_PER_DAY - now_secs_of_day + XVERIFY_RUN_AT_SECS_OF_DAY_IST
    }
}

/// IST seconds-of-day at which continuous trading actually begins (09:15:00).
///
/// Deliberately NOT [`TICK_PERSIST_START_SECS_OF_DAY_IST`] (09:00): the
/// persistence window opens 15 minutes early to capture the pre-open session,
/// during which no continuous trading happens and therefore EVERY instrument
/// is legitimately silent. Judging silence from 09:00 would page every
/// trading morning at ~09:01.
const CONTINUOUS_SESSION_START_SECS_OF_DAY_IST: u64 = 9 * 3_600 + 15 * 60;

/// True when `secs_of_day` (IST) is inside the window where an instrument is
/// EXPECTED to be ticking, so silence is evidence of a fault.
///
/// Narrower than the persistence window on purpose — see
/// [`CONTINUOUS_SESSION_START_SECS_OF_DAY_IST`] for why the 09:00–09:15
/// pre-open is excluded. Pure and total, so both boundaries are testable
/// without a clock.
#[must_use]
pub const fn is_within_market_hours_ist(secs_of_day: u64) -> bool {
    secs_of_day >= CONTINUOUS_SESSION_START_SECS_OF_DAY_IST
        && secs_of_day < TICK_PERSIST_END_SECS_OF_DAY_IST as u64
}

/// Current IST seconds-of-day.
#[must_use]
pub fn now_ist_secs_of_day() -> u64 {
    let ist = chrono::Utc::now().with_timezone(&tickvault_common::trading_calendar::ist_offset());
    let t = ist.time();
    u64::from(chrono::Timelike::num_seconds_from_midnight(&t))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tickvault_common::constants::{
        DISCONNECT_PACKET_SIZE, FULL_QUOTE_PACKET_SIZE, OI_PACKET_SIZE, PREVIOUS_CLOSE_PACKET_SIZE,
        QUOTE_PACKET_SIZE, TICKER_PACKET_SIZE,
    };
    // Test-only: production stopped importing this when the silence gate moved
    // from the persistence window (09:00) to the continuous session (09:15).
    // The tests still need it — proving 09:00 is OUTSIDE the gate is exactly
    // what pins that the pre-open cannot page.
    use std::collections::BTreeSet;
    use std::time::Instant;

    /// A calendar carrying one known NSE holiday, for the trading-day gate.
    fn synthetic_calendar() -> tickvault_common::trading_calendar::TradingCalendar {
        use tickvault_common::config::{NseHolidayEntry, TradingConfig};
        let cfg = TradingConfig {
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
            nse_mock_trading_dates: vec![],
        };
        tickvault_common::trading_calendar::TradingCalendar::from_config(&cfg)
            .expect("synthetic calendar builds")
    }

    /// A WEEKDAY NSE holiday is NOT a trading day — this is the exact shape
    /// that produced the false `silent=<universe>` page: EventBridge starts the
    /// box `MON-FRI`, the market is shut, and every seeded instrument crosses
    /// the silence floor at once.
    #[test]
    fn weekday_nse_holiday_is_not_a_trading_day() {
        let calendar = synthetic_calendar();
        // 2026-01-26 is a Monday AND a configured NSE holiday.
        let holiday = chrono::NaiveDate::from_ymd_opt(2026, 1, 26).expect("valid date");
        assert_eq!(
            chrono::Datelike::weekday(&holiday),
            chrono::Weekday::Mon,
            "the fixture must be a WEEKDAY holiday — a weekend proves nothing, \
             because the EventBridge schedule already excludes weekends"
        );
        assert!(
            !calendar.is_trading_day(holiday),
            "a weekday NSE holiday must not be treated as a trading day"
        );
        // Control: the next day is an ordinary trading Tuesday.
        let next = chrono::NaiveDate::from_ymd_opt(2026, 1, 27).expect("valid date");
        assert!(
            calendar.is_trading_day(next),
            "the day after the holiday must still page normally — this gate must \
             narrow the page, never disable it"
        );
    }

    /// With no calendar installed the gate FAILS OPEN.
    ///
    /// Direction matters and is not symmetric: a false page is noise, a
    /// suppressed page on a real trading day is undetected data loss on a feed
    /// whose protocol carries no sequence number and no snapshot-on-subscribe.
    #[test]
    fn silence_page_gate_fails_open_when_no_calendar_is_installed() {
        // This process may or may not have had a calendar installed by another
        // test; the invariant under test is that the ABSENT case allows the
        // page, so only assert when the cell is genuinely empty.
        if TRADING_CALENDAR.get().is_none() {
            assert!(
                silence_page_allowed_today(),
                "an uninstalled calendar must never suppress a silence page"
            );
        }
    }

    /// A real NSE-session second, well past the plausibility floor.
    const LTT_IST_SECS: u32 = 1_772_073_900;

    /// The UTC nanosecond instant that is EXACTLY simultaneous with
    /// `LTT_IST_SECS` — i.e. zero delivery lag.
    const SIMULTANEOUS_RECV_NANOS: i64 = (LTT_IST_SECS as i64 - 19_800) * 1_000_000_000;

    #[test]
    fn ws_lag_ms_subtracts_the_ist_offset_and_never_adds_it() {
        // THE test. `data-integrity.md` calls the WebSocket timestamp rule the
        // single most critical data-integrity rule in the repo: the exchange
        // stamp is IST-epoch, so comparing it to a UTC clock requires
        // SUBTRACTING 19,800 s. Adding instead would not look obviously broken
        // — it would report a steady 11-hour lag, which reads like a unit bug
        // rather than a sign error, and every percentile would be garbage.
        assert_eq!(
            ws_lag_ms(LTT_IST_SECS, SIMULTANEOUS_RECV_NANOS),
            Some(WsLag::Measured(0.0)),
            "a simultaneous tick must measure exactly zero lag"
        );

        // Pin the magnitude a sign error would produce, so the failure message
        // names the actual mistake instead of just showing two numbers.
        let wrong_direction = SIMULTANEOUS_RECV_NANOS + 2 * 19_800 * 1_000_000_000;
        assert_eq!(
            ws_lag_ms(LTT_IST_SECS, wrong_direction),
            Some(WsLag::Measured(39_600_000.0)),
            "39,600,000 ms = 11 h is the signature of a +19800 sign error"
        );
    }

    #[test]
    fn ws_lag_ms_measures_a_real_delay_in_milliseconds() {
        let recv = SIMULTANEOUS_RECV_NANOS + 250 * 1_000_000;
        assert_eq!(ws_lag_ms(LTT_IST_SECS, recv), Some(WsLag::Measured(250.0)));
        // The 46-second class this feed was retired for must render honestly.
        let recv_slow = SIMULTANEOUS_RECV_NANOS + 46_370 * 1_000_000;
        assert_eq!(
            ws_lag_ms(LTT_IST_SECS, recv_slow),
            Some(WsLag::Measured(46_370.0))
        );
    }

    #[test]
    fn ws_lag_ms_clamps_a_negative_rather_than_recording_it() {
        // Dhan sends LTT as whole SECONDS, so truncation alone can make a fast
        // delivery compute negative. Recording a negative would corrupt the
        // histogram; silently dropping it would hide a genuinely wrong host
        // clock. Clamp AND count is the only honest option.
        let early = SIMULTANEOUS_RECV_NANOS - 900 * 1_000_000;
        assert_eq!(ws_lag_ms(LTT_IST_SECS, early), Some(WsLag::ClampedNegative));
    }

    #[test]
    fn ws_lag_ms_excludes_an_implausible_timestamp_instead_of_recording_zero() {
        // A zero or garbage LTT is not a zero-latency tick. Folding it in as
        // 0.0 would drag every percentile toward zero and make a DEGRADING
        // feed look like it was getting faster — the false-OK class rule 11
        // forbids. `None` means the caller counts it as excluded.
        for garbage in [0u32, 1, 1_599_999_999] {
            assert_eq!(
                ws_lag_ms(garbage, SIMULTANEOUS_RECV_NANOS),
                None,
                "LTT {garbage} is below the plausibility floor and must be excluded"
            );
        }
        // The floor itself is inclusive-valid.
        assert!(ws_lag_ms(1_600_000_000, SIMULTANEOUS_RECV_NANOS).is_some());
    }

    #[test]
    fn record_ws_lag_uses_resolved_handles_and_never_allocates_per_tick() {
        // The first cut of `record_ws_lag` built its label with
        // `connection_index.to_string()`, which allocated a String AND — because
        // a non-literal label value drops the macro to its `vec![Label::new(..)]`
        // arm — a Vec, TWICE per tick, on the path this module's docs call
        // allocation-free. `DrainCounters` 800 lines above warns about exactly
        // that, in this same file, and it happened anyway.
        //
        // Worth recording: a PARALLEL session found and fixed the identical
        // defect on `main`, with the same reasoning. That implementation won
        // the merge because it derives its slot count from
        // `MAX_TOTAL_DHAN_CONNECTIONS` rather than a hardcoded 16 — one fewer
        // number to keep in sync with the operator's socket lock. This test was
        // rewritten against that API rather than kept alongside a duplicate.
        let handles = ws_lag_handles();
        assert!(
            std::ptr::eq(handles, ws_lag_handles()),
            "handles must be resolved once and reused, not rebuilt per call"
        );

        // Every socket slot has its OWN handle: folding two sockets onto one
        // would silently merge their percentiles and hide a single slow
        // connection, which is the one thing this metric exists to expose.
        for slot in 0..MAX_TOTAL_DHAN_CONNECTIONS as usize {
            let by_index = u8::try_from(slot).expect("the socket lock caps this well under u8");
            assert!(
                std::ptr::eq(
                    handles.histogram_for(by_index),
                    &handles.per_connection[slot]
                ),
                "connection {slot} must map to its own handle"
            );
        }

        // Out of range degrades into the counted `unknown` bucket rather than
        // allocating a fresh key on the hot path. Wrong bucket, bounded cost,
        // and COUNTED — the same fail-into-a-known-bucket choice
        // `DrainCounters::refused` makes.
        assert!(
            std::ptr::eq(handles.histogram_for(u8::MAX), &handles.unknown_connection),
            "an out-of-range index must degrade to the unknown bucket, never allocate"
        );

        // Source-scan: the allocating forms must not come back.
        let src = include_str!("dhan_feed_stack.rs");
        let body_start = src
            .find("pub fn record_ws_lag(")
            .expect("record_ws_lag must exist");
        let body = &src[body_start..body_start + 600];
        assert!(
            !body.contains("to_string()"),
            "record_ws_lag must not build label values on the hot path"
        );
        assert!(
            !body.contains("metrics::histogram!"),
            "record_ws_lag must use pre-resolved handles, not the macro"
        );
    }

    use tickvault_common::constants::TICK_PERSIST_START_SECS_OF_DAY_IST;
    use tickvault_common::types::SecurityId;

    fn instruments(n: usize) -> Vec<SubscribeInstrument> {
        (0..n)
            .map(|i| SubscribeInstrument {
                security_id: SecurityId::try_from(i).unwrap_or(SecurityId::MAX),
                segment: ExchangeSegment::NseFno,
            })
            .collect()
    }

    #[test]
    fn the_depth_writer_is_built_unconditionally_because_depth_attaches_late() {
        // THE BUG THIS PINS, which shipped and was caught before merge:
        // building the depth ingest only when the BOOT-time depth sets are
        // non-empty leaves it `None` forever, because main.rs passes empty
        // vectors by design — depth instruments come from the option-chain
        // leg via `attach_depth_when_available` after 09:16 IST.
        //
        // The result was sockets connected, frames arriving, every one
        // counted `depth_unconsumed` and DISCARDED, with no error anywhere:
        // the exact captured-then-thrown-away behaviour the depth work
        // existed to end, reintroduced by a guard meant to be tidy.
        //
        // A source scan rather than a runtime assertion because the
        // construction happens inside `run_dhan_feed_stack`, which needs a
        // live token manager and real sockets to reach.
        let src = include_str!("dhan_feed_stack.rs");
        let test_marker = concat!("#[cfg(", "test)]");
        let production = src.split(test_marker).next().unwrap_or(src);
        // Anchored on the BINDING itself, not on the predicate. The same
        // `params.depth_20_instruments.is_empty()` test appears a few lines
        // below to gate the late-attach SPAWN, where it is exactly right — a
        // blanket ban on the predicate would fail on correct code, and a guard
        // that fails for a reason unrelated to what it protects teaches the
        // next reader to delete it.
        let binding = production
            .lines()
            .find(|l| l.trim_start().starts_with("let depth_ingest"))
            .expect("the depth ingest binding must exist");
        assert!(
            binding.contains("Some(DepthIngest::new("),
            "the depth ingest must be built UNCONDITIONALLY — gating it on the \
             boot-time depth sets makes it None for the WHOLE session, because \
             those sets are always empty at boot and depth attaches later. \
             Found: {binding}"
        );
    }

    #[test]
    fn a_depth_frame_with_no_ingest_is_counted_as_a_wiring_bug_not_as_success() {
        // The `None` arm still exists and is still honest: it counts rather
        // than pretending. It should be unreachable in production now, which
        // is why its counter's meaning is "a wiring bug", not "no depth".
        let src = include_str!("dhan_feed_stack.rs");
        let test_marker = concat!("#[cfg(", "test)]");
        let production = src.split(test_marker).next().unwrap_or(src);
        assert!(
            production.contains("c.depth_unconsumed.increment(1)"),
            "the no-ingest arm must still COUNT — a silently dropped depth frame \
             is what this whole path exists to eliminate"
        );
    }

    #[test]
    fn both_depth_write_paths_consult_the_ingest_shed_gate_and_ticks_never_do() {
        // The gate is only a lever if BOTH paths read it. A version that
        // gated inline depth and forgot the dedicated feeds would shed the
        // cheap half and keep writing the expensive one, on a disk that is
        // already full — worse than not shedding at all, because the counters
        // would say shedding is working.
        let src = include_str!("dhan_feed_stack.rs");
        let test_marker = concat!("#[cfg(", "test)]");
        let production = src.split(test_marker).next().unwrap_or(src);

        assert!(
            production.contains("INGEST_SHED.allows_inline_depth()"),
            "the inline-depth append must be gated"
        );
        assert!(
            production.contains("INGEST_SHED.allows_dedicated_depth()"),
            "the depth-20 / depth-200 routing must be gated"
        );
        assert!(
            production.contains("c.shed_inline_depth.increment(1)")
                && production.contains("c.shed_dedicated_depth.increment(1)"),
            "a shed row must be COUNTED — silently writing less is the false-OK \
             this whole design exists to avoid"
        );

        // And the guarantee that makes shedding acceptable: no tick path may
        // ever consult the gate. If this ever fails, someone has taught the
        // box to drop prices to save disk, which is the one thing it must not
        // do.
        assert!(
            !production.contains("INGEST_SHED.allows_ticks"),
            "ticks are NEVER shed — no tick path may consult the gate"
        );
    }

    // -- I-P1-11 dedup + distinct-slot sizing -------------------------------

    fn inst(id: u64, seg: ExchangeSegment) -> SubscribeInstrument {
        SubscribeInstrument {
            security_id: SecurityId::from(id),
            segment: seg,
        }
    }

    // -- late top-up of contracts whose underlying priced after the dial -----

    /// A channel wide enough that a refusal in these tests always means the
    /// code refused, never that the buffer happened to be full.
    fn topup_slot(
        room: usize,
    ) -> (
        (tokio::sync::mpsc::Sender<LiveSubscriptionCommand>, usize),
        tokio::sync::mpsc::Receiver<LiveSubscriptionCommand>,
    ) {
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        ((tx, room), rx)
    }

    /// Unwraps the `Extend` payload a top-up sends.
    ///
    /// The channel carries a command enum since 2026-08-26 (it also carries
    /// `Swap`, for the per-minute at-the-money re-selection). A top-up only
    /// ever sends `Extend`, and a test that received a `Swap` here would be
    /// reporting a real defect, so this panics rather than returning empty.
    fn extended(cmd: LiveSubscriptionCommand) -> Vec<SubscribeInstrument> {
        match cmd {
            LiveSubscriptionCommand::Extend(more) => more,
            LiveSubscriptionCommand::Swap { .. } => {
                panic!("a top-up sent a Swap — it must only ever Extend")
            }
        }
    }

    #[test]
    fn top_up_late_contracts_sends_only_what_is_not_already_on_the_wire() {
        let mut sent = std::collections::HashSet::new();
        sent.insert(contract_identity(&inst(1, ExchangeSegment::NseFno)));
        sent.insert(contract_identity(&inst(2, ExchangeSegment::NseFno)));
        let (slot, mut rx) = topup_slot(100);
        let mut slots = [slot];
        let selection = [
            inst(1, ExchangeSegment::NseFno),
            inst(2, ExchangeSegment::NseFno),
            inst(3, ExchangeSegment::NseFno),
        ];

        let placed = top_up_late_contracts(&selection, &mut sent, &mut slots, 1, 10_000);

        assert_eq!(placed, 1, "only the instrument nobody has been told about");
        let got = rx.try_recv().expect("the delta must reach the channel");
        assert_eq!(
            extended(got),
            vec![inst(3, ExchangeSegment::NseFno)],
            "re-sending an already-live instrument is a silent double-subscribe, and Dhan \
             answers an over-limit subscribe with 804 by dropping the connection"
        );
    }

    #[test]
    fn top_up_late_contracts_is_idempotent_across_attempts() {
        let mut sent = std::collections::HashSet::new();
        let (slot, mut rx) = topup_slot(100);
        let mut slots = [slot];
        let selection = [inst(7, ExchangeSegment::NseFno)];

        assert_eq!(
            top_up_late_contracts(&selection, &mut sent, &mut slots, 1, 10_000),
            1
        );
        assert_eq!(
            top_up_late_contracts(&selection, &mut sent, &mut slots, 2, 10_000),
            0,
            "the loop re-selects every attempt; a second send of the same set is the exact \
             804 hazard this function exists to prevent"
        );
        assert!(rx.try_recv().is_ok());
        assert!(
            rx.try_recv().is_err(),
            "the second attempt must put NOTHING on the channel"
        );
    }

    #[test]
    fn top_up_late_contracts_keys_on_the_composite_not_the_id_alone() {
        // I-P1-11: the same numeric id in two segments is two instruments.
        let mut sent = std::collections::HashSet::new();
        sent.insert(contract_identity(&inst(42, ExchangeSegment::NseFno)));
        let (slot, mut rx) = topup_slot(100);
        let mut slots = [slot];
        let selection = [
            inst(42, ExchangeSegment::NseFno),
            inst(42, ExchangeSegment::BseFno),
        ];

        let placed = top_up_late_contracts(&selection, &mut sent, &mut slots, 1, 10_000);

        assert_eq!(
            placed, 1,
            "keying on security_id alone would withhold the BSE_FNO contract forever"
        );
        assert_eq!(
            extended(rx.try_recv().expect("a send")),
            vec![inst(42, ExchangeSegment::BseFno)]
        );
    }

    #[test]
    fn top_up_late_contracts_does_not_mark_a_refused_send_as_subscribed() {
        let mut sent = std::collections::HashSet::new();
        let (tx, rx) = tokio::sync::mpsc::channel::<LiveSubscriptionCommand>(1);
        drop(rx); // the connection task is gone: every send is refused
        let mut slots = [(tx, 100)];
        let selection = [inst(9, ExchangeSegment::NseFno)];

        let placed = top_up_late_contracts(&selection, &mut sent, &mut slots, 1, 10_000);

        assert_eq!(placed, 0);
        assert!(
            sent.is_empty(),
            "recording a REFUSED send as subscribed turns a transient failure into a \
             permanent hole — the instrument could never be offered again"
        );
    }

    #[test]
    fn top_up_late_contracts_spills_across_connections_by_room() {
        let mut sent = std::collections::HashSet::new();
        let (first, mut rx_first) = topup_slot(2);
        let (second, mut rx_second) = topup_slot(10);
        let mut slots = [first, second];
        let selection: Vec<_> = (1..=5)
            .map(|id| inst(id, ExchangeSegment::NseFno))
            .collect();

        let placed = top_up_late_contracts(&selection, &mut sent, &mut slots, 1, 10_000);

        assert_eq!(placed, 5);
        assert_eq!(
            extended(rx_first.try_recv().expect("first")).len(),
            2,
            "its room"
        );
        assert_eq!(
            extended(rx_second.try_recv().expect("second")).len(),
            3,
            "the rest"
        );
        assert_eq!(slots[0].1, 0, "room is decremented as sends succeed");
        assert_eq!(slots[1].1, 7);
    }

    #[test]
    fn top_up_late_contracts_places_nothing_when_every_connection_is_full() {
        let mut sent = std::collections::HashSet::new();
        let (slot, mut rx) = topup_slot(0);
        let mut slots = [slot];
        let selection = [inst(11, ExchangeSegment::NseFno)];

        assert_eq!(
            top_up_late_contracts(&selection, &mut sent, &mut slots, 1, 10_000),
            0
        );
        assert!(sent.is_empty());
        assert!(rx.try_recv().is_err());
    }

    /// No connection registered a top-up channel at all.
    ///
    /// Distinct from "every connection is at its cap" and NOT interchangeable
    /// with it: this is a wiring failure, and until 2026-08-22 both produced
    /// the same message blaming the 5 x 5,000 capacity budget. Pinned because
    /// the two send triage in opposite directions.
    #[test]
    fn top_up_late_contracts_survives_no_registered_connection() {
        let mut sent = std::collections::HashSet::new();
        let mut slots: [(
            tokio::sync::mpsc::Sender<
                tickvault_core::websocket::pool_supervisor::LiveSubscriptionCommand,
            >,
            usize,
        ); 0] = [];
        let selection = [inst(11, ExchangeSegment::NseFno)];

        assert_eq!(
            top_up_late_contracts(&selection, &mut sent, &mut slots, 1, 10_000),
            0,
            "nothing can be placed with nowhere to place it"
        );
        assert!(
            sent.is_empty(),
            "and nothing may be recorded as subscribed — recording here would lock these \
             contracts out of every later attempt"
        );
    }

    /// The unplaced diagnostic must name WHICH of its two causes applies.
    ///
    /// A source scan rather than a log capture: the value under test is the
    /// operator-facing wording, and a behavioural assertion on `tracing`
    /// output would pin the harness rather than the message.
    #[test]
    fn top_up_unplaced_error_separates_wiring_from_capacity() {
        // The test module is scanned OUT before searching. `include_str!`
        // embeds this whole file, so a needle spelled here matches ITSELF and
        // the guard can never fail. Written that way first, on 2026-08-22,
        // and caught only because the bite-proof was actually run: collapsing
        // the branch to `if false` left the test green.
        let full = include_str!("dhan_feed_stack.rs");
        let test_marker = concat!("#[cfg(", "test)]");
        let src = full.split(test_marker).next().unwrap_or(full);
        assert!(
            src.contains("let cause = if slots.is_empty()"),
            "the unplaced error must branch on whether any connection exists"
        );
        assert!(
            src.contains("a WIRING failure, not a"),
            "the empty-slots branch must say it is NOT a capacity problem"
        );
        assert!(
            src.contains("every main-feed connection is at its cap"),
            "the genuine capacity branch must survive"
        );
    }

    #[test]
    fn top_up_late_contracts_refuses_a_delta_that_atm_drift_would_explain() {
        // One underlying priced since the dial, so at most 102 contracts are
        // legitimate. A 500-instrument delta means the ATM windows of already
        // subscribed stocks SLID as the market opened — subscribing it would
        // spend the remaining slots on re-centering nobody authorized.
        let mut sent = std::collections::HashSet::new();
        let (slot, mut rx) = topup_slot(10_000);
        let mut slots = [slot];
        let selection: Vec<_> = (1..=500)
            .map(|id| inst(id, ExchangeSegment::NseFno))
            .collect();

        let placed = top_up_late_contracts(
            &selection,
            &mut sent,
            &mut slots,
            1,
            MAX_CONTRACTS_PER_LATE_UNDERLYING,
        );

        assert_eq!(placed, 0, "refused whole, never partially");
        assert!(
            sent.is_empty(),
            "a refused top-up must leave the session exactly as it dialed"
        );
        assert!(rx.try_recv().is_err(), "nothing may reach the wire");
    }

    #[test]
    fn top_up_late_contracts_accepts_a_delta_inside_the_late_underlying_budget() {
        let mut sent = std::collections::HashSet::new();
        let (slot, _rx) = topup_slot(10_000);
        let mut slots = [slot];
        let selection: Vec<_> = (1..=MAX_CONTRACTS_PER_LATE_UNDERLYING as u64)
            .map(|id| inst(id, ExchangeSegment::NseFno))
            .collect();

        let placed = top_up_late_contracts(
            &selection,
            &mut sent,
            &mut slots,
            1,
            MAX_CONTRACTS_PER_LATE_UNDERLYING,
        );

        assert_eq!(
            placed, MAX_CONTRACTS_PER_LATE_UNDERLYING,
            "exactly at the budget is legitimate, not refused"
        );
    }

    #[test]
    fn top_up_late_contracts_dedups_within_one_payload() {
        // The `sent` filter cannot catch this: neither copy is on the wire
        // yet, so both survive the set difference. The pool dial gets a
        // second dedup layer from the planner; the top-up bypasses it.
        let mut sent = std::collections::HashSet::new();
        let (slot, mut rx) = topup_slot(100);
        let mut slots = [slot];
        let selection = [
            inst(5, ExchangeSegment::NseFno),
            inst(5, ExchangeSegment::NseFno),
            inst(6, ExchangeSegment::NseFno),
        ];

        let placed = top_up_late_contracts(&selection, &mut sent, &mut slots, 1, 10_000);

        assert_eq!(placed, 2, "the repeat must not reach the wire");
        let got = extended(rx.try_recv().expect("a send"));
        assert_eq!(got.len(), 2);
        assert_eq!(
            got.iter().filter(|i| i.security_id == 5).count(),
            1,
            "a duplicate inside one payload is a double-subscribe, which Dhan answers with \
             804 by dropping the connection"
        );
    }

    #[test]
    fn max_contracts_per_late_underlying_is_the_atm_window_on_both_legs() {
        assert_eq!(
            MAX_CONTRACTS_PER_LATE_UNDERLYING,
            (2 * tickvault_common::constants::STOCK_OPTION_ATM_STRIKES_EACH_SIDE + 1) * 2
        );
        assert_eq!(
            MAX_CONTRACTS_PER_LATE_UNDERLYING, 102,
            "ATM +/- 25, CE + PE"
        );
    }

    #[test]
    fn the_top_up_budget_is_derived_from_underlyings_that_newly_priced() {
        // Source scan: the budget must come from the FALL in
        // `underlyings_without_spot` since the dial. Deriving it from
        // anything else (the raw count, a constant, the delta itself) makes
        // the drift refusal vacuous.
        let full = include_str!("dhan_feed_stack.rs");
        let test_marker = concat!("#[cfg(", "test)]");
        let src = full.split(test_marker).next().unwrap_or(full);
        assert!(
            src.contains("let newly_priced = dial_without_spot")
                && src.contains(".saturating_sub(contracts.underlyings_without_spot)"),
            "the budget must be the FALL in underlyings_without_spot since the dial"
        );
        assert!(
            src.contains("if newly_priced > 0 {"),
            "no underlying priced since the dial means nothing legitimate to add — the \
             top-up must not even look, because any delta present is pure drift"
        );
        assert!(
            src.contains("newly_priced.saturating_mul(MAX_CONTRACTS_PER_LATE_UNDERLYING)"),
            "the budget is per-underlying, so it must scale with how many priced"
        );
    }

    #[test]
    fn contract_topup_cutoff_sits_after_the_readiness_deadline_and_the_open() {
        assert!(
            CONTRACT_TOPUP_CUTOFF_IST_SECS > PREOPEN_READY_DEADLINE_IST_SECS,
            "chasing a late-priced stock only makes sense AFTER the first dial, which the \
             readiness deadline bounds"
        );
        assert!(
            u64::from(CONTRACT_TOPUP_CUTOFF_IST_SECS) > CONTINUOUS_SESSION_START_SECS_OF_DAY_IST,
            "a stock that has not traded by 09:15 is exactly the case this chases"
        );
        assert_eq!(CONTRACT_TOPUP_CUTOFF_IST_SECS, 34_200, "09:30:00 IST");
    }

    #[test]
    fn contract_dial_retains_its_top_up_channels() {
        // Source scan: `out_topups: None` on the contract dial is what left
        // ~780 late-priced options unsubscribed every session, and it is a
        // one-word regression to reintroduce.
        let full = include_str!("dhan_feed_stack.rs");
        let test_marker = concat!("#[cfg(", "test)]");
        let src = full.split(test_marker).next().unwrap_or(full);
        assert!(
            src.contains("out_topups: Some(&mut live_topups)"),
            "the contract half must keep its senders so a late top-up can reach those sockets"
        );
        assert_eq!(
            src.matches("out_topups: None").count(),
            1,
            "exactly ONE dial legitimately has nothing to add later: depth, whose sets are \
             final. If this is 2, the contract dial lost its channels again."
        );
    }

    #[test]
    fn dedup_subscribe_set_removes_repeats_and_reports_how_many() {
        let set = vec![
            inst(13, ExchangeSegment::IdxI),
            inst(25, ExchangeSegment::IdxI),
            inst(13, ExchangeSegment::IdxI), // exact repeat
        ];
        let (unique, dupes) = dedup_subscribe_set(&set);
        assert_eq!(unique.len(), 2);
        assert_eq!(
            dupes, 1,
            "the count must be reported, never silently absorbed"
        );
    }

    #[test]
    fn dedup_subscribe_set_keys_on_the_composite_never_on_the_id_alone() {
        // I-P1-11: Dhan reuses one numeric id across segments. 13 is NIFTY on
        // IDX_I and a DIFFERENT instrument on NSE_EQ. Deduping on the id alone
        // would unsubscribe a real instrument — strictly worse than the
        // duplicate it set out to remove.
        let set = vec![
            inst(13, ExchangeSegment::IdxI),
            inst(13, ExchangeSegment::NseEquity),
        ];
        let (unique, dupes) = dedup_subscribe_set(&set);
        assert_eq!(
            unique.len(),
            2,
            "same id, different segments = TWO instruments"
        );
        assert_eq!(dupes, 0);
    }

    #[test]
    fn dedup_subscribe_set_preserves_order_so_shard_assignment_is_deterministic() {
        let set = vec![
            inst(3, ExchangeSegment::NseFno),
            inst(1, ExchangeSegment::NseFno),
            inst(3, ExchangeSegment::NseFno),
            inst(2, ExchangeSegment::NseFno),
        ];
        let (unique, _) = dedup_subscribe_set(&set);
        let ids: Vec<u64> = unique.iter().map(|i| i.security_id).collect();
        assert_eq!(ids, vec![3, 1, 2], "first occurrence wins, order preserved");
    }

    #[test]
    fn dedup_subscribe_set_of_an_empty_set_is_empty_and_reports_nothing() {
        let (unique, dupes) = dedup_subscribe_set(&[]);
        assert!(unique.is_empty());
        assert_eq!(dupes, 0);
    }

    #[test]
    fn distinct_fold_slots_does_not_double_count_across_pools() {
        // The bug this replaces: capacity was
        // `main.len() + depth_20.len() + depth_200.len()`. An option that is
        // subscribed on the main feed AND on depth-20 is ONE aggregator slot
        // — the aggregator keys on (Feed, security_id, segment) — but the sum
        // counted it twice, inflating the sizing toward the 25,000 ceiling for
        // capacity the process never needed.
        let main = vec![
            inst(100, ExchangeSegment::NseFno),
            inst(101, ExchangeSegment::NseFno),
        ];
        let d20 = vec![inst(100, ExchangeSegment::NseFno)]; // also on the main feed
        let d200 = vec![inst(102, ExchangeSegment::NseFno)];
        assert_eq!(
            main.len() + d20.len() + d200.len(),
            4,
            "the old sizing would have said 4"
        );
        assert_eq!(
            distinct_fold_slots(&main, &d20, &d200),
            3,
            "there are only THREE distinct instruments"
        );
    }

    #[test]
    fn distinct_fold_slots_still_separates_the_same_id_in_two_segments() {
        let main = vec![inst(13, ExchangeSegment::IdxI)];
        let d20 = vec![inst(13, ExchangeSegment::NseEquity)];
        assert_eq!(
            distinct_fold_slots(&main, &d20, &[]),
            2,
            "collapsing these would size the fold for one instrument and run two"
        );
    }

    #[test]
    fn a_pool_of_pure_duplicates_plans_as_one_instrument() {
        // End to end: 6,000 copies of one instrument would need 2 connections
        // by the raw count and 1 by the real one. More importantly it proves
        // the dedup runs INSIDE build_feed_stack_plan, not merely that the
        // helper exists.
        let mut pool = PoolSupervisor::new();
        let now = std::time::Instant::now();
        let set = vec![inst(13, ExchangeSegment::IdxI); 6_000];
        let plan = build_feed_stack_plan(&mut pool, now, &set, &[], &[])
            .expect("6,000 copies of ONE instrument must fit one connection");
        let total: usize = plan
            .connections
            .iter()
            .filter(|c| c.slot.endpoint == DhanEndpointType::MainFeed)
            .map(|c| c.guard.len())
            .sum();
        assert_eq!(total, 1, "the wire must carry the instrument ONCE");
    }

    #[test]
    fn duplicates_never_push_a_fitting_set_over_the_connection_limit() {
        // 25,000 unique instruments exactly fill 5 x 5,000. Adding a duplicate
        // of one of them takes the raw count to 25,001, which `plan_pool`
        // refuses for the WHOLE endpoint — every socket dark. Dedup is what
        // stops a repeated entry from causing a total outage.
        let mut pool = PoolSupervisor::new();
        let now = std::time::Instant::now();
        let mut set = instruments(25_000);
        set.push(set[0]);
        assert_eq!(set.len(), 25_001);
        let plan = build_feed_stack_plan(&mut pool, now, &set, &[], &[])
            .expect("a duplicate must not take the main feed dark");
        let total: usize = plan
            .connections
            .iter()
            .filter(|c| c.slot.endpoint == DhanEndpointType::MainFeed)
            .map(|c| c.guard.len())
            .sum();
        assert_eq!(total, 25_000);
    }

    // -- pool spreading (operator directive 2026-08-12: use all 16) ---------

    /// A set that FITS one connection must still spread across the authorized
    /// five.
    ///
    /// This is the whole point of the change. 4,565 main-feed instruments fit
    /// one 5,000-instrument socket, so packing opened exactly ONE connection
    /// and left four authorized ones idle — which is how "16 authorized" was
    /// really 7 in practice.
    #[test]
    fn test_main_feed_packs_its_first_pass_so_contracts_still_fit() {
        let mut pool = PoolSupervisor::new();
        let now = std::time::Instant::now();
        // The real resolved-master size on the live box, 2026-08-12.
        let plan = build_feed_stack_plan(&mut pool, now, &instruments(4565), &[], &[])
            .expect("4565 instruments must plan cleanly");
        assert_eq!(
            plan.count_for(DhanEndpointType::MainFeed),
            1,
            "4,565 fits ONE 5,000-instrument socket. Spreading it across all \
             five took the whole pool, so the ~20,000-contract second pass was \
             refused — and because MainFeed is planned first, that refusal \
             aborted depth planning too. Packing pass 1 is what ends with all \
             five main-feed sockets carrying data."
        );
        assert_eq!(
            pool.total_open(),
            1,
            "four connections must remain free for the contract pass"
        );
    }

    /// The Dhan per-connection cap is absolute and spreading must never breach it.
    ///
    /// Spreading widens shards only when a set fits in FEWER connections than
    /// are authorized, so the width can only shrink relative to the cap — but
    /// this asserts the invariant directly rather than trusting that argument.
    #[test]
    fn all_sixteen_sockets_carry_data_at_the_authorized_scale() {
        // THE END-TO-END ARITHMETIC, asserted rather than argued. This is the
        // operator's actual requirement — 16 connections, all carrying data —
        // and until 2026-08-20 the answer was 6, with depth structurally
        // unable to plan at all.
        //
        // Two passes, exactly as the live lane runs them: spots at boot, then
        // contracts once post-open prices exist.
        let max = usize::from(tickvault_core::websocket::pool_budget::MAX_MAIN_FEED_CONNECTIONS);

        // Pass 1 — boot spots.
        let mut pool = PoolSupervisor::new();
        let now = std::time::Instant::now();
        let boot = build_feed_stack_plan(&mut pool, now, &instruments(4_565), &[], &[])
            .expect("the spot universe must plan");
        assert_eq!(boot.count_for(DhanEndpointType::MainFeed), 1);

        // Pass 2 — contracts sized by what pass 1 left, plus both depth pools.
        // The SAME pool, so `admit` is stateful exactly as in production.
        let used = boot.count_for(DhanEndpointType::MainFeed);
        let room = remaining_main_feed_capacity(used);
        let attach = build_feed_stack_plan(
            &mut pool,
            now,
            &instruments(room),
            &instruments(250),
            &instruments(5),
        )
        .expect(
            "the contract + depth pass must PLAN — a BudgetRefused here is the \
             defect that cost depth an entire session, because MainFeed is \
             planned first and its refusal aborts depth too",
        );

        assert_eq!(attach.count_for(DhanEndpointType::MainFeed), 4, "contracts");
        assert_eq!(attach.count_for(DhanEndpointType::Depth20), 5);
        assert_eq!(attach.count_for(DhanEndpointType::Depth200), 5);

        // 5 main-feed (1 spots + 4 contracts) + 5 depth-20 + 5 depth-200 = 15,
        // plus the order-update socket dialed by the REST stack = 16.
        assert_eq!(used + attach.count_for(DhanEndpointType::MainFeed), max);
        assert_eq!(
            pool.total_open(),
            15,
            "fifteen market-data sockets; the sixteenth is order-update, dialed \
             by dhan_rest_stack and counted against its own endpoint budget"
        );
    }

    #[test]
    fn test_spread_shard_width_never_exceeds_the_dhan_cap() {
        for n in [1usize, 2, 49, 50, 51, 200, 249, 250, 4565, 25_000] {
            let mut pool = PoolSupervisor::new();
            let now = std::time::Instant::now();
            let Ok(plan) = build_feed_stack_plan(&mut pool, now, &instruments(n), &[], &[]) else {
                // Beyond 5 x 5,000 the planner refuses outright — that arm is
                // covered by the PoolTooSmall path, not by this invariant.
                continue;
            };
            let conns = plan.count_for(DhanEndpointType::MainFeed);
            assert!(
                conns <= 5,
                "{n} instruments planned {conns} connections, over the cap"
            );
            if conns > 0 {
                let widest = n.div_ceil(conns);
                assert!(
                    widest <= 5000,
                    "{n} instruments over {conns} connections gives shards of \
                     {widest}, above Dhan's 5,000 per-connection cap"
                );
            }
        }
    }

    /// Never open a connection with nothing on it.
    ///
    /// With 4 depth-200 instruments and a 1-per-connection cap, the answer is
    /// 4 sockets — not 5 with an empty one. An empty subscribe is a socket
    /// that reports healthy while carrying nothing, which is exactly the
    /// false-OK the scope lock forbids.
    #[test]
    fn test_spread_never_opens_an_empty_connection() {
        let mut pool = PoolSupervisor::new();
        let now = std::time::Instant::now();
        let plan = build_feed_stack_plan(&mut pool, now, &[], &[], &instruments(4))
            .expect("4 depth-200 instruments must plan cleanly");
        assert_eq!(
            plan.count_for(DhanEndpointType::Depth200),
            4,
            "4 instruments at 1-per-connection is 4 sockets — never 5 with one \
             carrying nothing"
        );
    }

    /// Depth-20 reaches all five without widening the strike selection.
    ///
    /// 84 instruments packed at 50-per-connection gave 2. Spread across the
    /// authorized 5 gives shards of 17 — well inside the cap, and it needs no
    /// deep-OTM strikes whose order books never move.
    #[test]
    fn test_depth_20_spreads_the_live_84_across_all_five() {
        let mut pool = PoolSupervisor::new();
        let now = std::time::Instant::now();
        let plan = build_feed_stack_plan(&mut pool, now, &[], &instruments(84), &[])
            .expect("84 depth-20 instruments must plan cleanly");
        assert_eq!(
            plan.count_for(DhanEndpointType::Depth20),
            5,
            "the live 84-instrument depth-20 set must use all 5 connections"
        );
    }

    /// A set genuinely too large still fails closed.
    ///
    /// Spreading must not weaken the refusal: beyond 5 x 5,000 the planner
    /// still refuses the WHOLE pool rather than silently truncating.
    #[test]
    fn test_oversize_set_still_refuses_rather_than_truncating() {
        let mut pool = PoolSupervisor::new();
        let now = std::time::Instant::now();
        let err = build_feed_stack_plan(&mut pool, now, &instruments(25_001), &[], &[])
            .expect_err("25,001 instruments exceed 5 x 5,000 and must refuse");
        assert!(
            matches!(err, FeedStackPlanError::PoolTooSmall { .. }),
            "expected PoolTooSmall, got {err:?}"
        );
    }

    // -- the gate -----------------------------------------------------------

    #[test]
    fn test_feed_stack_gate_is_shut_when_the_config_flag_is_off() {
        // NOTE (2026-08-26): base.toml now carries dhan_enabled = TRUE; this
        // test passes `false` explicitly to exercise the gate arm, and does
        // not describe the shipped config. Was written when both files said
        // `dhan_enabled = false`, which was true from the 2026-07-13
        // retirement until the 2026-08-11 flip.
        assert_eq!(
            feed_stack_gate(false, Some(DHAN_LIVE_FEED_ENV_ON)),
            FeedStackGate::DisabledByConfig
        );
        assert_eq!(
            feed_stack_gate(false, None),
            FeedStackGate::DisabledByConfig
        );
    }

    #[test]
    fn test_gate_is_shut_without_the_environment_opt_in() {
        // This is the gate that makes the lane default-OFF BY CONSTRUCTION:
        // `FeedsConfig`'s struct default for `dhan_enabled` is `true`, so a
        // config that failed to carry a `[feeds]` section must still not open
        // sixteen sockets.
        assert_eq!(feed_stack_gate(true, None), FeedStackGate::DisabledByEnv);
        for junk in ["", "0", "true", "yes", "on", "TRUE", " 1", "1 "] {
            assert_eq!(
                feed_stack_gate(true, Some(junk)),
                FeedStackGate::DisabledByEnv,
                "only the exact literal `1` may open the lane, not {junk:?}"
            );
        }
    }

    #[test]
    fn test_gate_opens_only_when_both_gates_agree() {
        let gate = feed_stack_gate(true, Some(DHAN_LIVE_FEED_ENV_ON));
        assert_eq!(gate, FeedStackGate::Enabled);
        assert!(gate.is_enabled());
        assert!(!FeedStackGate::DisabledByConfig.is_enabled());
        assert!(!FeedStackGate::DisabledByEnv.is_enabled());
    }

    #[test]
    fn test_gate_labels_are_distinct() {
        let labels: BTreeSet<&str> = [
            FeedStackGate::Enabled,
            FeedStackGate::DisabledByConfig,
            FeedStackGate::DisabledByEnv,
        ]
        .iter()
        .map(|g| g.as_str())
        .collect();
        assert_eq!(labels.len(), 3);
    }

    // -- the universe -------------------------------------------------------

    #[test]
    fn test_hardcoded_index_universe_is_the_pinned_set_and_nothing_else() {
        // Q3 of the 2026-07-13 amendment stands: hardcoded security ids only,
        // no CSV download, no parser.
        let u = hardcoded_index_universe();
        assert_eq!(u.len(), SPOT_1M_REST_INDICES.len());
        assert!(
            u.iter().all(|i| i.segment == ExchangeSegment::IdxI),
            "every pinned instrument is an index value"
        );
        let ids: BTreeSet<SecurityId> = u.iter().map(|i| i.security_id).collect();
        assert!(ids.contains(&13), "NIFTY");
        assert!(ids.contains(&25), "BANKNIFTY");
        assert!(ids.contains(&51), "SENSEX");
        assert_eq!(ids.len(), u.len(), "no duplicate security ids");
    }

    #[test]
    fn test_hardcoded_universe_tracks_the_rest_leg_table() {
        // Shared source of truth: the live lane and the REST legs can never
        // drift onto different security ids for the same index.
        let u = hardcoded_index_universe();
        for (idx, (security_id, _)) in SPOT_1M_REST_INDICES.iter().enumerate() {
            assert_eq!(u.get(idx).map(|i| i.security_id), Some(*security_id));
        }
    }

    // -- planning -----------------------------------------------------------

    #[test]
    fn test_build_feed_stack_plan_packs_the_index_universe_onto_one_connection() {
        // CHANGED 2026-08-20 (main-feed PACKS again, depth still spreads —
        // scope-lock amendment of that date). Four index SIDs fit one 5,000-
        // instrument socket, and packing pass 1 is what leaves the other four
        // connections for the contract pass. Under spread these four SIDs took
        // four sockets and the contracts were refused.
        let mut pool = PoolSupervisor::new();
        let plan = build_feed_stack_plan(
            &mut pool,
            Instant::now(),
            &hardcoded_index_universe(),
            &[],
            &[],
        )
        .expect("four indices plan cleanly");
        assert_eq!(plan.len(), 1);
        assert_eq!(plan.count_for(DhanEndpointType::MainFeed), 1);
        assert_eq!(plan.count_for(DhanEndpointType::Depth20), 0);
        assert_eq!(plan.count_for(DhanEndpointType::Depth200), 0);
        assert_eq!(pool.total_open(), 1);
    }

    /// The arithmetic that makes splitting the contract dial a BAD trade.
    ///
    /// Dialing the ~1,380 price-independent contracts on their own connection
    /// first looks like an obvious win — they need no spot price, so why hold
    /// them behind a stock-pricing quorum? Because `plan_pool` cannot pack a
    /// later set onto a socket already dialed, so that connection's unused
    /// slots are stranded and come straight out of the ATM window.
    ///
    /// This pins the numbers rather than the prose. If the connection count or
    /// the per-connection cap ever changes such that the split becomes
    /// harmless, this test fails and the comment above gets revisited — which
    /// is the only way a rejected-alternative note stays trustworthy.
    #[test]
    fn splitting_the_contract_dial_would_strand_slots_and_shrink_the_window() {
        let per_conn = usize::try_from(
            tickvault_core::websocket::pool_budget::MAIN_FEED_INSTRUMENTS_PER_CONNECTION,
        )
        .expect("per-connection cap fits usize");

        // Spot packs onto one connection, so the contract half starts with
        // four whole connections free.
        let spot_connections = 1usize;
        let single_dial = remaining_main_feed_capacity(spot_connections);

        // A split would spend one of those four on ~1,380 contracts.
        let price_independent = 1_380usize;
        assert!(
            price_independent < per_conn,
            "the price-independent set fits in ONE connection, which is exactly \
             why splitting strands the rest of it"
        );
        let after_split = remaining_main_feed_capacity(spot_connections + 1);

        let stranded = single_dial
            .saturating_sub(price_independent)
            .saturating_sub(after_split);
        assert!(
            stranded > 3_000,
            "expected a split to strand thousands of slots, computed {stranded} — \
             if this is now small, the comment above rejecting the split is stale"
        );

        // Two legs per strike, so the stranded slots cost half that many
        // strikes each side, spread across the priced underlyings.
        assert!(
            stranded / 2 > 1_500,
            "the stranded slots are option LEGS; halving them is the strike count \
             the ATM window loses"
        );
    }
    #[test]
    fn test_plan_count_for_packs_the_main_feed_and_spreads_depth() {
        let mut pool = PoolSupervisor::new();
        // CHANGED 2026-08-20: the MAIN FEED packs, DEPTH still spreads.
        //   12,001 main-feed -> ceil(12001/5000) = 3 conns (cap 5,000)
        //      101 depth-20   -> 5 conns of 21    (cap 50, spread)
        // depth-200 is capped at 1 instrument per connection, so 3
        // instruments is 3 sockets — bounded by the set, never padded.
        let plan = build_feed_stack_plan(
            &mut pool,
            Instant::now(),
            &instruments(12_001),
            &instruments(101),
            &instruments(3),
        )
        .expect("all three shards fit inside the authorized pools");
        assert_eq!(plan.count_for(DhanEndpointType::MainFeed), 3);
        assert_eq!(plan.count_for(DhanEndpointType::Depth20), 5);
        assert_eq!(plan.count_for(DhanEndpointType::Depth200), 3);
        assert_eq!(plan.len(), 11);
        assert_eq!(pool.total_open(), 11);
    }

    #[test]
    fn test_plan_at_the_full_authorized_shape_is_exactly_fifteen_market_data_sockets() {
        // 5 main-feed + 5 depth-20 + 5 depth-200. The sixteenth authorized
        // socket is order-update, which this stack does not own.
        let mut pool = PoolSupervisor::new();
        let plan = build_feed_stack_plan(
            &mut pool,
            Instant::now(),
            &instruments(25_000),
            &instruments(250),
            &instruments(5),
        )
        .expect("this is exactly the authorized ceiling");
        assert_eq!(plan.count_for(DhanEndpointType::MainFeed), 5);
        assert_eq!(plan.count_for(DhanEndpointType::Depth20), 5);
        assert_eq!(plan.count_for(DhanEndpointType::Depth200), 5);
        assert_eq!(plan.len(), 15);
        assert_eq!(pool.total_open(), 15);
    }

    #[test]
    fn test_plan_refuses_an_oversized_main_feed_set_before_any_dial() {
        // 25,001 needs a sixth main-feed connection. Dhan would not reject it —
        // it would silently kill the oldest — so we refuse locally instead.
        let mut pool = PoolSupervisor::new();
        let err = build_feed_stack_plan(&mut pool, Instant::now(), &instruments(25_001), &[], &[])
            .expect_err("a sixth main-feed connection must be refused");
        assert!(matches!(
            err,
            FeedStackPlanError::PoolTooSmall {
                endpoint: DhanEndpointType::MainFeed,
                needed: 6,
                available: 5,
                ..
            }
        ));
        assert_eq!(
            pool.total_open(),
            0,
            "a refused plan must reserve nothing at all"
        );
    }

    #[test]
    fn test_plan_refuses_an_oversized_depth_set() {
        let mut pool = PoolSupervisor::new();
        // 251 depth-20 instruments = 6 connections; only 5 are authorized.
        assert!(matches!(
            build_feed_stack_plan(&mut pool, Instant::now(), &[], &instruments(251), &[]),
            Err(FeedStackPlanError::PoolTooSmall {
                endpoint: DhanEndpointType::Depth20,
                ..
            })
        ));
        // 6 depth-200 instruments = 6 connections; only 5 are authorized.
        let mut pool2 = PoolSupervisor::new();
        assert!(matches!(
            build_feed_stack_plan(&mut pool2, Instant::now(), &[], &[], &instruments(6)),
            Err(FeedStackPlanError::PoolTooSmall {
                endpoint: DhanEndpointType::Depth200,
                ..
            })
        ));
    }

    #[test]
    fn test_plan_places_every_instrument_exactly_once() {
        let mut pool = PoolSupervisor::new();
        let plan = build_feed_stack_plan(&mut pool, Instant::now(), &instruments(7_777), &[], &[])
            .expect("inside the pool");
        let total: usize = plan.connections.iter().map(|c| c.guard.len()).sum();
        assert_eq!(total, 7_777, "sharding must not drop or duplicate anything");
        // CHANGED 2026-08-20: the main feed packs, so 7,777 is 2 conns of
        // 5,000 and 2,777. Conservation above is the invariant that actually
        // matters and is unchanged by either policy.
        assert_eq!(plan.count_for(DhanEndpointType::MainFeed), 2);
    }

    #[test]
    fn test_plan_gives_every_connection_a_distinct_global_index() {
        // Distinct global indices are what give each connection its own
        // reconnect stagger, which is the thundering-herd guard.
        let mut pool = PoolSupervisor::new();
        let plan = build_feed_stack_plan(
            &mut pool,
            Instant::now(),
            &instruments(25_000),
            &instruments(250),
            &instruments(5),
        )
        .expect("the authorized ceiling");
        let indices: BTreeSet<u8> = plan
            .connections
            .iter()
            .map(|c| c.slot.global_index)
            .collect();
        assert_eq!(indices.len(), 15, "every connection needs its own stagger");
        assert!(
            indices.iter().all(|i| *i < 16),
            "global indices must stay inside the sixteen-slot space"
        );
    }

    #[test]
    fn test_plan_of_an_empty_universe_opens_nothing() {
        let mut pool = PoolSupervisor::new();
        let plan = build_feed_stack_plan(&mut pool, Instant::now(), &[], &[], &[])
            .expect("an empty universe is a valid, empty plan");
        assert!(plan.is_empty());
        assert_eq!(plan.len(), 0);
        assert_eq!(pool.total_open(), 0);
    }

    #[test]
    fn test_planned_guards_start_needing_a_subscribe() {
        let mut pool = PoolSupervisor::new();
        let plan = build_feed_stack_plan(
            &mut pool,
            Instant::now(),
            &hardcoded_index_universe(),
            &[],
            &[],
        )
        .expect("inside the pool");
        let guard = plan
            .connections
            .first()
            .map(|c| &c.guard)
            .expect("one connection was planned");
        assert!(guard.needs_resubscribe());
        assert_eq!(guard.generation(), 0);
        assert_eq!(guard.endpoint(), DhanEndpointType::MainFeed);
    }

    // -- spawn --------------------------------------------------------------

    #[tokio::test]
    async fn test_spawn_dhan_feed_stack_is_refused_when_the_lane_is_disabled() {
        // The default state on every boot since 2026-07-13: no task, no
        // socket, no behaviour change.
        let handle = spawn_dhan_feed_stack(DhanFeedStackParams {
            dhan_enabled: false,
            // A disabled lane never reaches the re-fold, which is exactly why
            // main.rs still drops the batch loudly when the gate is closed.
            wal_replay_live_feed: Vec::new(),
            main_feed_instruments: hardcoded_index_universe(),
            depth_20_instruments: Vec::new(),
            depth_200_instruments: Vec::new(),
            questdb: QuestDbConfig {
                host: "questdb.invalid".to_string(),
                http_port: 9000,
                pg_port: 8812,
                ilp_port: 9009,
            },
            // Deliberately `Some`-less: proving the CONFIG gate refuses first,
            // before anything looks at the durability floor.
            spill: None,
            // Likewise irrelevant here — the config gate is checked before any
            // of the three floors, and this test pins that ordering.
            rest_fold_writes_dhan_candles: false,
            // A real state object, default-constructed: the disabled lane must
            // spawn nothing, and must therefore leave this flag untouched at
            // its `false` default. Asserted below, so this test also pins that
            // a refused lane never claims to be running.
            feed_runtime: Arc::new(tickvault_api::feed_state::FeedRuntimeState::default()),
            feed_health: Arc::new(tickvault_common::feed_health::FeedHealthRegistry::new()),
            shutdown: Arc::new(tokio::sync::Notify::new()),
            calendar: Arc::new(synthetic_calendar()),
        });
        assert!(handle.is_none(), "a disabled lane must spawn nothing");
    }

    #[test]
    fn test_dual_candle_writer_refusal_is_wired_before_any_socket() {
        // Source-order pin for the exclusivity floor.
        //
        // The live lane and the REST fold both seal into `candles_<tf>`
        // stamped `feed='dhan'`, and the dedup key — ts, security_id, segment,
        // feed — has no column that distinguishes them. Every column matches
        // for the same minute of the same instrument, so QuestDB upserts one
        // over the other with no error and no counter. The 15:31 comparator
        // then reads that same table as its LIVE side, which would make it
        // compare the REST record against the REST record and agree every
        // time.
        //
        // Asserting on source order rather than behaviour because the refusal
        // returns before constructing anything observable — the property that
        // matters is that it happens BEFORE the sockets, not that it logs.
        let src = include_str!("dhan_feed_stack.rs");
        let test_marker = concat!("#[cfg(", "test)]");
        let production = src.split(test_marker).next().unwrap_or(src);

        let refusal = production
            .find("rest_fold_writes_dhan_candles {")
            .expect("the exclusivity floor must exist in the bring-up");
        let verification = production
            .find("spawn_daily_crossverify(&params.main_feed_instruments)")
            .expect("the verification floor must exist");
        // Anchor on the CALL SITE, not the socket-opening statement.
        //
        // This used to search for `run_connection(socket`, which lives inside
        // the dial loop. That worked while the loop was inline in
        // `run_dhan_feed_stack`, but the loop was extracted into
        // `dial_planned_connections` (2026-08-14) so both dial phases — main
        // feed at open, depth after 09:16 — share one body. The helper is
        // DEFINED above `run_dhan_feed_stack` and CALLED below the refusal, so
        // a text-position search now reports the dial as "first" purely because
        // of where the function sits in the file.
        //
        // The invariant this test exists for is about EXECUTION order, and the
        // call site is what carries it. Anchoring on the definition made the
        // guard sensitive to code motion that changes nothing it cares about —
        // the same class of mistake as the earlier bare-"dial" anchor that
        // matched a doc comment.
        let dial = production
            .find("MAIN-FEED-DIAL-SITE")
            .expect("the dial call site must exist inside the bring-up");

        assert!(
            refusal < verification,
            "the exclusivity check must come before the comparator is armed — arming a \
             comparator that would then read its own input is worse than not arming it"
        );
        assert!(
            refusal < dial,
            "the exclusivity check must come before ANY socket is dialed"
        );
    }

    #[test]
    fn test_base_url_refuses_the_order_update_endpoint() {
        // The market-data endpoints resolve; order-update must not. That
        // socket belongs to the REST stack and its spawn is retired — a URL
        // here would put it one match arm away from being dialed twice.
        assert_eq!(
            base_url_for(DhanEndpointType::MainFeed),
            Some(DHAN_MAIN_FEED_WS_BASE_URL)
        );
        assert_eq!(
            base_url_for(DhanEndpointType::Depth20),
            Some(DHAN_TWENTY_DEPTH_WS_BASE_URL)
        );
        assert_eq!(
            base_url_for(DhanEndpointType::Depth200),
            Some(DHAN_TWO_HUNDRED_DEPTH_WS_BASE_URL)
        );
        assert_eq!(
            base_url_for(DhanEndpointType::OrderUpdate),
            None,
            "the live-feed stack must never be able to dial the order-update socket"
        );
    }

    #[test]
    fn test_current_feed_token_is_none_without_a_registered_manager() {
        // Fail-closed: no token manager means no credential, and the bring-up
        // refuses rather than dialing with a blank. (A test binary never
        // registers one, so this is the real state here.)
        assert!(
            current_feed_token().is_none(),
            "no registered token manager must yield no token, never an empty string"
        );
    }

    /// Builds one 16-byte ticker packet (response code 2) for `security_id`.
    fn ticker_packet(security_id: u32, ltp: f32, ltt: u32) -> [u8; 16] {
        let mut p = [0u8; 16];
        p[0] = 2; // response code: ticker
        p[1] = 16; // message length
        p[3] = 0; // exchange segment: IDX_I
        p[4..8].copy_from_slice(&security_id.to_le_bytes());
        p[8..12].copy_from_slice(&ltp.to_le_bytes());
        p[12..16].copy_from_slice(&ltt.to_le_bytes());
        p
    }

    /// Builds one depth-20 side-packet: 12-byte header + 20 × 16-byte levels.
    ///
    /// Level `i` gets price `100 + i`, quantity `10 * (i + 1)` and `i + 1`
    /// orders, so a test can assert that level ORDER survived — a writer that
    /// reversed or offset the levels would still produce 20 rows.
    fn depth20_packet(security_id: u32, segment: u8, feed_code: u8) -> Vec<u8> {
        let mut p = vec![0u8; 12 + 20 * 16];
        let len = u16::try_from(p.len()).expect("332 fits u16");
        p[0..2].copy_from_slice(&len.to_le_bytes());
        p[2] = feed_code;
        p[3] = segment;
        p[4..8].copy_from_slice(&security_id.to_le_bytes());
        for i in 0..20usize {
            let base = 12 + i * 16;
            let price = 100.0_f64 + i as f64;
            let qty = u32::try_from(10 * (i + 1)).expect("small");
            let orders = u32::try_from(i + 1).expect("small");
            p[base..base + 8].copy_from_slice(&price.to_le_bytes());
            p[base + 8..base + 12].copy_from_slice(&qty.to_le_bytes());
            p[base + 12..base + 16].copy_from_slice(&orders.to_le_bytes());
        }
        p
    }

    /// A depth-200 packet with a CALLER-CHOSEN row count.
    ///
    /// Depth-200 is the variable-length half of the protocol: the row count is
    /// read from header bytes 8..12 and the packet is `12 + rows * 16` bytes,
    /// so the same code path that is fixed-at-20 for depth-20 is data-driven
    /// here. Until 2026-08-15 the entire 400-rows-per-update pool had ZERO
    /// end-to-end drain tests — every existing test built a depth-20 packet —
    /// so the branch that reads that count was undriven.
    fn depth200_packet(security_id: u32, segment: u8, feed_code: u8, rows: u32) -> Vec<u8> {
        let n = rows as usize;
        let mut p = vec![0u8; 12 + n * 16];
        let len = u16::try_from(p.len()).expect("<= 3212 fits u16");
        p[0..2].copy_from_slice(&len.to_le_bytes());
        p[2] = feed_code;
        p[3] = segment;
        p[4..8].copy_from_slice(&security_id.to_le_bytes());
        // The field depth-20 uses as a sequence is the ROW COUNT here.
        p[8..12].copy_from_slice(&rows.to_le_bytes());
        for i in 0..n {
            let base = 12 + i * 16;
            let price = 500.0_f64 + i as f64 * 0.25;
            let qty = u32::try_from(7 * (i + 1)).expect("small");
            let orders = u32::try_from(i + 1).expect("small");
            p[base..base + 8].copy_from_slice(&price.to_le_bytes());
            p[base + 8..base + 12].copy_from_slice(&qty.to_le_bytes());
            p[base + 12..base + 16].copy_from_slice(&orders.to_le_bytes());
        }
        p
    }

    fn depth_frame(bytes: Vec<u8>, endpoint: DhanEndpointType, seq: u64) -> CapturedFrame {
        CapturedFrame {
            seq,
            endpoint,
            connection_index: 5,
            received_at: std::time::Instant::now(),
            bytes: bytes::Bytes::from(bytes),
        }
    }

    #[test]
    fn a_depth20_bid_packet_becomes_twenty_rows_in_level_order() {
        let mut depth = DepthIngest::for_test();
        let frame = depth_frame(depth20_packet(13, 0, 41), DhanEndpointType::Depth20, 7);
        let out = drain_depth_frame(
            &mut depth,
            &frame,
            1_779_355_000_000_000_000,
            DepthFeedKind::Twenty,
            counters(),
        );
        assert_eq!(out.rows, 20, "every level is a row — nothing is sampled");
        assert_eq!(out.refused, 0);
        assert_eq!(depth.pending_rows(), 20);
    }

    #[test]
    fn bid_and_ask_are_separate_packets_and_both_are_kept() {
        // Bid and ask arrive as SEPARATE packets. A consumer that kept only
        // one would produce a book with no other side and no error.
        let mut depth = DepthIngest::for_test();
        let mut bytes = depth20_packet(13, 0, 41);
        bytes.extend_from_slice(&depth20_packet(13, 0, 51));
        let frame = depth_frame(bytes, DhanEndpointType::Depth20, 7);
        let out = drain_depth_frame(
            &mut depth,
            &frame,
            1_779_355_000_000_000_000,
            DepthFeedKind::Twenty,
            counters(),
        );
        assert_eq!(out.rows, 40, "20 bid + 20 ask, stacked in ONE frame");
        assert_eq!(out.refused, 0);
    }

    #[test]
    fn an_unmappable_segment_is_refused_not_written_under_a_guess() {
        // Segment 200 maps to no known segment. Writing it anyway would store
        // the levels against the wrong instrument identity (I-P1-11), which
        // looks like data and is worse than a gap.
        let mut depth = DepthIngest::for_test();
        let frame = depth_frame(depth20_packet(13, 200, 41), DhanEndpointType::Depth20, 7);
        let out = drain_depth_frame(
            &mut depth,
            &frame,
            1_779_355_000_000_000_000,
            DepthFeedKind::Twenty,
            counters(),
        );
        assert_eq!(out.rows, 0);
        assert_eq!(out.refused, 1, "the refusal is COUNTED, never silent");
        assert_eq!(depth.pending_rows(), 0);
    }

    #[test]
    fn a_truncated_depth_frame_is_counted_rather_than_read_as_a_short_book() {
        // A frame whose tail we could not parse must not read as "that was all
        // the packets" — that is how a partial book passes for a complete one.
        let mut depth = DepthIngest::for_test();
        let mut bytes = depth20_packet(13, 0, 41);
        bytes.truncate(12 + 5 * 16); // header + 5 levels, then nothing
        let frame = depth_frame(bytes, DhanEndpointType::Depth20, 7);
        let out = drain_depth_frame(
            &mut depth,
            &frame,
            1_779_355_000_000_000_000,
            DepthFeedKind::Twenty,
            counters(),
        );
        assert_eq!(
            out.rows, 0,
            "a short packet is refused whole, not partially"
        );
        assert!(out.refused >= 1);
    }

    #[test]
    fn the_frame_sequence_is_reused_as_capture_seq_so_replay_collapses() {
        // REWRITTEN 2026-08-15. The previous version asserted `out.rows == 20`
        // twice and never read capture_seq — it conceded in its own comment
        // that the property held "by construction", which is a claim, not an
        // assertion. Replacing the derivation with a fresh mint left it green
        // while every WAL replay doubled the book.
        let mut depth = DepthIngest::for_test();
        let frame = depth_frame(
            depth20_packet(13, 0, 41),
            DhanEndpointType::Depth20,
            556_007_424,
        );
        let out = drain_depth_frame(
            &mut depth,
            &frame,
            1_779_355_000_000_000_000,
            DepthFeedKind::Twenty,
            counters(),
        );
        assert_eq!(out.rows, 20);
        let first = depth.pending_ilp();
        assert!(
            first.contains("capture_seq=556007424i"),
            "capture_seq must be DERIVED from frame.seq, not minted, not minted; got: {first}"
        );

        // Re-folding the SAME frame must reproduce the SAME sequence, so the
        // database collapses the replay onto the originals instead of storing
        // a second book. A mint would produce a different value here.
        let mut replay = DepthIngest::for_test();
        let again = drain_depth_frame(
            &mut replay,
            &frame,
            1_779_355_000_000_000_000,
            DepthFeedKind::Twenty,
            counters(),
        );
        assert_eq!(again.rows, 20);
        assert_eq!(
            replay.pending_ilp(),
            first,
            "a replayed frame must emit byte-identical rows, or it lands as a duplicate book"
        );
    }

    #[test]
    fn two_packets_in_one_frame_get_distinct_capture_seqs() {
        // A frame STACKS packets. `capture_seq` is a DEDUP key column, so if
        // every packet in a frame carried the bare `frame.seq`, two packets
        // sharing (security_id, segment, side) would match on all eight key
        // columns and QuestDB would upsert one away — invisibly, because
        // `out.rows` counts ILP appends, not DB acceptance.
        //
        // The packet index occupies the low bits `next_frame_seq` reserves, so
        // consecutive packets differ by exactly 1.
        let mut depth = DepthIngest::for_test();
        let mut bytes = depth20_packet(13, 0, 41);
        bytes.extend_from_slice(&depth20_packet(13, 0, 51));
        let frame = depth_frame(bytes, DhanEndpointType::Depth20, 556_007_424);
        let out = drain_depth_frame(
            &mut depth,
            &frame,
            1_779_355_000_000_000_000,
            DepthFeedKind::Twenty,
            counters(),
        );
        assert_eq!(out.rows, 40, "20 bid + 20 ask");
        let ilp = depth.pending_ilp();
        assert!(
            ilp.contains("capture_seq=556007424i"),
            "packet 0 keeps the frame's own sequence: {ilp}"
        );
        assert!(
            ilp.contains("capture_seq=556007425i"),
            "packet 1 must occupy the NEXT reserved packet slot, not reuse 4242: {ilp}"
        );
    }

    #[test]
    fn a_depth200_packet_emits_exactly_its_header_row_count() {
        // The variable-length half of the protocol. A fixed-20 assumption here
        // would silently truncate or over-read every real depth-200 book.
        for rows in [1u32, 5, 200] {
            let mut depth = DepthIngest::for_test();
            let frame = depth_frame(
                depth200_packet(13, 0, 41, rows),
                DhanEndpointType::Depth200,
                556_007_424,
            );
            let out = drain_depth_frame(
                &mut depth,
                &frame,
                1_779_355_000_000_000_000,
                DepthFeedKind::TwoHundred,
                counters(),
            );
            assert_eq!(
                out.rows, rows as u64,
                "a depth-200 packet declaring {rows} rows must emit exactly {rows}"
            );
            assert_eq!(out.refused, 0);
            let ilp = depth.pending_ilp();
            assert!(
                ilp.contains("depth_kind=d200"),
                "the d200 discriminator is what stops the two pools overwriting each \
                 other in the shared table: {ilp}"
            );
            assert!(
                !ilp.contains("depth_kind=d20,"),
                "a depth-200 packet must never be labelled d20: {ilp}"
            );
        }
    }

    #[test]
    fn a_depth200_row_count_above_the_protocol_max_is_refused_not_truncated() {
        // 201 exceeds the 200-level protocol ceiling. Truncating to 200 would
        // publish a book we cannot vouch for; refusing keeps the gap honest.
        let mut depth = DepthIngest::for_test();
        let frame = depth_frame(
            depth200_packet(13, 0, 41, 201),
            DhanEndpointType::Depth200,
            556_007_424,
        );
        let out = drain_depth_frame(
            &mut depth,
            &frame,
            1_779_355_000_000_000_000,
            DepthFeedKind::TwoHundred,
            counters(),
        );
        assert_eq!(out.rows, 0, "an over-max row count writes nothing");
        assert!(out.refused >= 1, "and the refusal is COUNTED, never silent");
    }

    #[test]
    fn a_depth200_packet_declaring_zero_rows_writes_nothing_and_stays_quiet() {
        // Zero rows is an EMPTY BOOK, not corruption — an instrument with no
        // resting orders. It must write nothing, and it must not be counted as
        // a refusal either, or a legitimately empty book reads as a fault.
        let mut depth = DepthIngest::for_test();
        let frame = depth_frame(
            depth200_packet(13, 0, 41, 0),
            DhanEndpointType::Depth200,
            556_007_424,
        );
        let out = drain_depth_frame(
            &mut depth,
            &frame,
            1_779_355_000_000_000_000,
            DepthFeedKind::TwoHundred,
            counters(),
        );
        assert_eq!(out.rows, 0);
        assert_eq!(depth.pending_rows(), 0);
    }

    #[test]
    fn an_absurd_or_non_finite_depth_price_is_refused_per_row_not_per_batch() {
        // The whole point of the per-ROW gate: without it a single poisoned
        // level fails the FLUSH, and `discard_pending` then wipes every good
        // row buffered alongside it — one bad level costs every other
        // instrument's book. Refusing the row keeps the other 19.
        let mut bytes = depth20_packet(13, 0, 41);
        // Level 0 gets NaN, level 1 gets f32::MAX-scale absurdity.
        bytes[12..20].copy_from_slice(&f64::NAN.to_le_bytes());
        bytes[28..36].copy_from_slice(&1.0e30_f64.to_le_bytes());
        let mut depth = DepthIngest::for_test();
        let frame = depth_frame(bytes, DhanEndpointType::Depth20, 556_007_424);
        let out = drain_depth_frame(
            &mut depth,
            &frame,
            1_779_355_000_000_000_000,
            DepthFeedKind::Twenty,
            counters(),
        );
        assert_eq!(out.rows, 18, "the 18 good levels still land");
        assert_eq!(
            out.refused, 2,
            "both poisoned levels are counted, not silent"
        );
        let ilp = depth.pending_ilp();
        assert!(!ilp.contains("NaN"), "no NaN may reach the wire: {ilp}");
    }

    #[test]
    fn a_zero_priced_depth_level_is_kept_because_it_is_an_absent_level_not_corruption() {
        // depth-20 is a FIXED 20 levels, so an illiquid contract with three
        // real bids still emits 20 rows and the rest are legitimately all-zero.
        // Refusing them would count normal book shape as corruption AND delete
        // the operator's own view of how deep a book actually is.
        let mut bytes = depth20_packet(13, 0, 41);
        for i in 3..20usize {
            let base = 12 + i * 16;
            bytes[base..base + 8].copy_from_slice(&0.0_f64.to_le_bytes());
        }
        let mut depth = DepthIngest::for_test();
        let frame = depth_frame(bytes, DhanEndpointType::Depth20, 556_007_424);
        let out = drain_depth_frame(
            &mut depth,
            &frame,
            1_779_355_000_000_000_000,
            DepthFeedKind::Twenty,
            counters(),
        );
        assert_eq!(out.rows, 20, "all 20 slots are kept, empty ones included");
        assert_eq!(out.refused, 0, "an empty level is NOT a refusal");
    }

    #[test]
    fn a_negative_depth_price_is_refused() {
        // A price cannot be below zero; unlike 0.0 there is no reading under
        // which this is a legitimate absent level.
        let mut bytes = depth20_packet(13, 0, 41);
        bytes[12..20].copy_from_slice(&(-1.0_f64).to_le_bytes());
        let mut depth = DepthIngest::for_test();
        let frame = depth_frame(bytes, DhanEndpointType::Depth20, 556_007_424);
        let out = drain_depth_frame(
            &mut depth,
            &frame,
            1_779_355_000_000_000_000,
            DepthFeedKind::Twenty,
            counters(),
        );
        assert_eq!(out.rows, 19);
        assert_eq!(out.refused, 1);
    }

    #[test]
    fn bid_and_ask_reach_the_row_under_the_right_side_label() {
        // The consequence of getting this wrong is an INVERTED ORDER BOOK — the
        // worst failure this protocol has. Every prior assertion was
        // `out.rows == 40`, which is equally true with the two arms swapped.
        let mut depth = DepthIngest::for_test();
        let mut bytes = depth20_packet(13, 0, 41); // 41 = bid
        bytes.extend_from_slice(&depth20_packet(13, 0, 51)); // 51 = ask
        let frame = depth_frame(bytes, DhanEndpointType::Depth20, 7);
        let out = drain_depth_frame(
            &mut depth,
            &frame,
            1_779_355_000_000_000_000,
            DepthFeedKind::Twenty,
            counters(),
        );
        assert_eq!(out.rows, 40);
        let ilp = depth.pending_ilp();
        assert!(ilp.contains("side=bid"), "bid packet lost its label: {ilp}");
        assert!(ilp.contains("side=ask"), "ask packet lost its label: {ilp}");
        assert!(
            ilp.contains("depth_kind=d20"),
            "a depth-20 frame must be labelled d20 — the discriminator is what \
             stops the two pools overwriting each other: {ilp}"
        );
        assert!(
            !ilp.contains("depth_kind=d200"),
            "a depth-20 frame must never be labelled d200: {ilp}"
        );
    }

    #[test]
    fn a_depth_frame_with_no_ingest_wired_counts_as_unconsumed_not_as_success() {
        // REWRITTEN 2026-08-15. The previous body built a `DepthFrameOutcome`
        // with `default()` and asserted its three fields were zero — i.e. that
        // the derive returns what the derive returns. It never called
        // `drain_depth_frame` and never touched the `None`-ingest arm it is
        // named for, so the wiring bug it claims to catch could ship freely.
        // What is asserted now: a wired ingest actually produces rows, so the
        // `None` arm is distinguishable from the success path by an observable
        // difference rather than by inspection.
        let frame = depth_frame(depth20_packet(13, 0, 41), DhanEndpointType::Depth20, 7);
        let out = drain_depth_frame(
            &mut DepthIngest::for_test(),
            &frame,
            1_779_355_000_000_000_000,
            DepthFeedKind::Twenty,
            counters(),
        );
        assert_eq!(out.rows, 20, "with an ingest wired, rows land");

        // HONEST LIMIT, stated rather than faked: the `None` arm itself lives
        // inline in `run_frame_drain`'s select loop and cannot be reached from
        // a unit test without standing up the whole drain. It is instead made
        // UNREACHABLE by construction — `DepthIngest::new` is called
        // unconditionally at stack build (it was gated on a boot-time
        // instrument list that `main.rs` passes empty by design, which silently
        // discarded every depth frame until 2026-08-15). That unconditional
        // construction is what `depth_wiring_guard` pins; this test pins the
        // other half, that a wired ingest is not itself a no-op.
    }

    #[test]
    fn test_ingest_folds_a_tick_and_reports_it_honestly() {
        // Asserts the FOLD, not just that a function returned.
        //
        // The earlier version of this test sent a frame into the drain and
        // asserted only that the drain terminated within 5 seconds. It would
        // have passed on a drain that discarded every frame — which is exactly
        // the class of defect this lane shipped with (seals computed and
        // thrown away, ticks buffered and never flushed). A test whose name
        // claims more than its body checks is worse than no test: it converts
        // an unknown into a false green.
        let mut ingest = LiveIngest::new(TickWriter::for_test(Feed::Dhan), 4);
        let packet = ticker_packet(13, 23_146.45, 1_779_355_000);
        let parsed = dispatch_frame(&packet, 1_779_355_000_000_000_000)
            .expect("a well-formed ticker packet must parse");
        let ParsedFrame::Tick(tick) = parsed else {
            panic!("response code 2 must dispatch to a Tick");
        };

        let outcome = ingest.ingest_tick(&tick, 42, 1_779_355_000_000);

        assert!(
            matches!(outcome, IngestOutcome::Folded { .. }),
            "a valid in-session tick must fold, got {outcome:?}"
        );
        assert_eq!(
            ingest.pending_rows(),
            1,
            "the folded tick must be buffered for the writer — a fold that \
             appends nothing is a fold that persists nothing"
        );
        assert_eq!(
            ingest.seq_refused(),
            0,
            "a representable sequence must not be refused"
        );
    }

    /// The cross-verification counter must be able to tell "we checked" from
    /// "we ran and proved nothing".
    ///
    /// This comparison is the revived Dhan feed's ONLY ground truth, and a
    /// `compared == 0` day is the exact false-OK this repository has retired
    /// twice. The comparator detects it properly -- `Blind` is a first-class
    /// outcome, `is_pass()` is false for it, and a vacuous run fires a coded
    /// `error!`. What was missing was DELIVERY: every surface flattened the
    /// distinction.
    ///
    ///   * the counter counted a vacuous run as `ran`, under a doc comment
    ///     that read "anything other than `ran` means the candles were never
    ///     checked" -- a promise the label could not keep;
    ///   * `WS-GAP-03`, the code the vacuous `error!` carries, is not one of
    ///     the 18 alarmed error codes, so the line pages nobody;
    ///   * the two audit tables it writes have no console query, no QuestDB
    ///     view, no dashboard widget and no runbook mention -- their only
    ///     other reader is `partition_manager`, which DELETES their
    ///     partitions on retention.
    ///
    /// The label split is the part that costs nothing: no new metric name, no
    /// new alarm, no operator quote. The remaining surfaces are recorded above
    /// rather than quietly fixed, because an alarm needs a dated operator
    /// quote per `dhan-rest-only-noise-lock-2026-07-14.md` §3.
    /// The daily audit row must carry EVERY total the comparison produced.
    ///
    /// A mapping that drops or transposes a column is the one failure mode
    /// this table cannot survive and no integration test would catch: the row
    /// lands, the count looks right, and the trend it exists to show is
    /// silently wrong. Distinct values per field so a transposition cannot
    /// pass by coincidence.
    #[test]
    fn the_daily_row_carries_every_comparison_total() {
        use tickvault_storage::dhan_live_crossverify_persistence::{
            DhanLiveXverifyCellFinding, DhanLiveXverifyCellKind, DhanLiveXverifyOutcome,
        };

        let finding = DhanLiveXverifyCellFinding {
            run_ts_ist_nanos: 1_724_000_000_000_000_000,
            trading_date_ist_nanos: 1_723_900_000_000_000_000,
            security_id: 13,
            segment: "IDX_I".to_owned(),
            minute_ts_ist_nanos: 1_723_950_000_000_000_000,
            kind: DhanLiveXverifyCellKind::Diverged,
            field: "close",
            live_value: 100.5,
            rest_value: 100.25,
            live_volume: 7,
            rest_volume: 9,
            diff_paise: 25,
        };
        let c = crate::dhan_live_crossverify::DayComparison {
            outcome: DhanLiveXverifyOutcome::Diverged,
            findings: vec![finding.clone()],
            instruments: 11,
            minutes_compared: 22,
            cells_diverged: 33,
            missing_live: 44,
            missing_live_traded: 40,
            missing_live_zero_volume: 4,
            missing_rest: 55,
            tail_unsealed: 66,
            out_of_session: 77,
            noise_p50_paise: 88,
            noise_p95_paise: 99,
            noise_max_paise: 111,
            volume_cells: 122,
            volume_exact: 133,
            volume_capture_p50_pct: 94,
            volume_capture_p05_pct: 61,
            volume_capture_min_pct: 12,
        };

        let row = xverify_daily_row(&c, 5);
        assert_eq!(row.instruments, 11);
        assert_eq!(row.minutes_compared, 22);
        assert_eq!(row.cells_diverged, 33);
        assert_eq!(row.missing_live, 44);
        assert_eq!(row.missing_rest, 55);
        assert_eq!(row.tail_unsealed, 66);
        assert_eq!(row.out_of_session, 77);
        assert_eq!(row.noise_p50_paise, 88);
        assert_eq!(row.noise_p95_paise, 99);
        assert_eq!(row.noise_max_paise, 111);
        assert_eq!(row.tolerance_paise, 5);
        assert_eq!(row.outcome, DhanLiveXverifyOutcome::Diverged);

        // The stamps come from the findings, NOT from `now()`, so a rerun
        // upserts onto the same row instead of appending a second verdict for
        // the same day an hour later.
        assert_eq!(row.run_ts_ist_nanos, finding.run_ts_ist_nanos);
        assert_eq!(row.trading_date_ist_nanos, finding.trading_date_ist_nanos);

        // A vacuous run has no findings and therefore no stamp to borrow.
        // It must still produce a row — "we could not measure today" is a
        // fact worth keeping — and it must not fabricate a timestamp.
        let blind = crate::dhan_live_crossverify::DayComparison {
            outcome: DhanLiveXverifyOutcome::Blind,
            findings: Vec::new(),
            minutes_compared: 0,
            ..c
        };
        let blind_row = xverify_daily_row(&blind, 0);
        assert_eq!(blind_row.outcome, DhanLiveXverifyOutcome::Blind);
        assert_eq!(blind_row.minutes_compared, 0);
    }

    /// The comment that said the findings "are already persisted" was false
    /// for the entire life of the feature. This pins that the wiring which
    /// makes it true is actually present — in all three places it has to be,
    /// because any one of them missing puts the system straight back to
    /// logging a verdict into the void.
    #[test]
    fn the_cross_verification_findings_are_actually_persisted() {
        let src = include_str!("dhan_feed_stack.rs");
        assert!(
            src.contains("persist_xverify_report(&deps.questdb, &report"),
            "the cross-verification run must call the persister; without this call the \
             feed's only ground-truth check is a log line again"
        );
        assert!(
            !src.contains("findings are\n                    // already persisted"),
            "the retracted false comment must not return"
        );

        // The DDL must run at boot, or ILP auto-creates both tables WITHOUT
        // their DEDUP keys and a rerun appends duplicate verdicts instead of
        // replacing them.
        let boot = include_str!("main.rs");
        assert!(
            boot.contains("ensure_dhan_live_crossverify_tables"),
            "boot must create the cross-verification audit tables; ILP would otherwise \
             auto-create them without the DEDUP keys that make a rerun idempotent"
        );

        // And the write-side config must reach the task.
        assert!(
            src.contains("pub questdb: tickvault_common::config::QuestDbConfig"),
            "CrossverifyDeps must carry the ILP write config"
        );
    }

    /// The 2026-08-26 loss, pinned at the call site.
    ///
    /// That session's 764,003 findings built a 207,965,278-byte ILP buffer
    /// against the server's 104,857,600 cap; the single flush was refused and
    /// the poisoned-buffer defence discarded every row, so the feed's only
    /// ground truth has no record for the day.
    ///
    /// Two halves, and BOTH are load-bearing. The chunked flush stops the
    /// buffer growing without bound — but it also converts "no rows today"
    /// into "most rows today", which is an improvement only while the
    /// shortfall is visible. A caller that chunked and then ignored the
    /// failures would ship a silently-short audit table, which reads as a
    /// complete one and is strictly worse than the honest total loss it
    /// replaced.
    #[test]
    fn the_xverify_persist_drains_in_chunks_and_reports_what_it_lost() {
        let src = include_str!("dhan_feed_stack.rs");

        let body_start = src
            .find("fn persist_xverify_report(")
            .expect("the persister must exist");
        // Bound the slice to the FUNCTION, not to end-of-file. This test's own
        // string literals below contain every marker it looks for, so an
        // unbounded slice would match itself and pass while production code had
        // lost the call entirely — a guard that reads its own source is the
        // purest form of the false-OK this file keeps finding.
        let body_end = body_start
            + 1
            + src[body_start + 1..]
                .find("\n}\n")
                .expect("the persister must end at column 0");
        let body = &src[body_start..body_end];
        assert!(
            !body.contains("fn the_xverify_persist_drains"),
            "the scanned slice has swallowed this test — it would then match its \
             own assertions and pass vacuously"
        );

        assert!(
            body.contains("flush_if_full(&mut writer, &mut batch_errors)"),
            "the append loop must drain in batches. Appending every finding and \
             flushing once is what built a 207,965,278-byte buffer on \
             2026-08-26 and lost all 764,003 rows to a refused flush"
        );
        // BOTH bounds, not either. Rows give a predictable batch size; BYTES
        // is the quantity that actually breached, and the only one that
        // survives a row shape getting wider.
        assert!(
            body.contains("w.pending() >= PERSIST_BATCH_ROWS"),
            "the row bound must survive"
        );
        assert!(
            body.contains("w.flush_if_large()"),
            "the BYTE bound must survive: a row bound alone re-creates the \
             'the rows can never get wider' assumption one layer down, which \
             is what the batch-size note itself warns about"
        );

        let counted = body
            .find("*errs += 1")
            .expect("a refused batch must be counted, not swallowed");
        let reported = body
            .find("batch_errors > 0")
            .expect("the count must reach the degraded-run condition");
        let logged = body
            .find("batch_errors,")
            .expect("the count must be a field on the partial-write error line");
        assert!(
            counted < reported && reported < logged,
            "the batch-failure count must be incremented, then gate the \
             partial-write verdict, then be logged — a count that never \
             reaches the verdict makes a short audit table look complete"
        );
    }

    #[test]
    fn xverify_counter_separates_a_measured_run_from_a_vacuous_one() {
        let src = include_str!("dhan_feed_stack.rs");

        assert!(
            src.contains("\"outcome\" => \"vacuous\""),
            "the vacuous label must exist -- without it a run that compared zero \
             minutes is indistinguishable from one that checked the whole day"
        );
        assert!(
            src.contains("\"outcome\" => \"measured\""),
            "the success label must say `measured`, not `ran`: a vacuous run also ran"
        );
        assert!(
            !src.contains("\"outcome\" => \"ran\""),
            "the ambiguous `ran` label must not come back"
        );

        // The split must be DRIVEN by is_vacuous(), not by anything a later
        // edit could let drift from the logged `vacuous =` field.
        let at = src
            .find("counters().xverify_vacuous.increment(1)")
            .expect("the vacuous arm must exist");
        let window = &src[at.saturating_sub(200)..at];
        assert!(
            window.contains("c.is_vacuous()"),
            "the vacuous counter must be gated on is_vacuous(), so the counter and \
             the log field cannot disagree"
        );
    }

    /// The two cross-verify `error!` lines carry a `source` DISCRIMINATOR.
    ///
    /// Without it these lines are unreachable by any alarm and unfindable by
    /// any triage query. `WS-GAP-03` is emitted from 25 sites in this file —
    /// dial failures, reconnects, pool-supervisor events — so the only filter
    /// shape that can single one out is the three-condition
    /// code + level + source form that
    /// `dhan-rest-only-noise-lock-2026-07-14.md` §2.3d-i settled on after a
    /// bare-code filter was approved and then found wrong. That section is
    /// the precedent; this test is what stops the field being dropped by a
    /// later edit, which would silently un-write a future alarm.
    ///
    /// The values must also be DISTINCT: "the check ran and measured nothing"
    /// and "the check could not run" have different causes and different
    /// remedies, and collapsing them to one label would merge two independent
    /// failures into one series — the same defect
    /// `fold_counters::the_two_labelled_extremes_are_separate_handles`
    /// guards on the metric side.
    ///
    /// This test does NOT claim an alarm exists. None does — the alarm needs
    /// a dated operator quote per §3 of that file, and the counter route is
    /// blocked separately (the metric is in neither EMF selector copy). What
    /// is asserted here is only that the field a future alarm must match on
    /// is present and unambiguous.
    #[test]
    fn the_xverify_error_lines_carry_a_source_an_alarm_can_match_on() {
        let src = include_str!("dhan_feed_stack.rs");

        for (marker, label) in [
            ("counters().xverify_vacuous.increment(1)", "xverify_vacuous"),
            ("counters().xverify_failed.increment(1)", "xverify_failed"),
        ] {
            assert!(
                src.contains(marker),
                "the {label} counter arm must exist — the log field is only half \
                 the signal"
            );

            // Anchor on the EMITTED field, not on a byte distance from the
            // counter. The two are ~70 lines apart in the vacuous arm and
            // adjacent in the failed one, so any fixed forward window is a
            // number that has to be re-tuned every time the prose above the
            // emit grows — and a window that silently became too short would
            // pass by finding nothing to check.
            let field = format!("source = \"{label}\"");
            let at = src
                .match_indices(&field)
                // Skip the filter-shape comment, which spells the same field
                // as `$.source = "…"` inside a CloudWatch pattern.
                .find(|(i, _)| !src[..*i].ends_with("$."))
                .map(|(i, _)| i)
                .unwrap_or_else(|| {
                    panic!(
                        "the {label} error! must carry `source = \"{label}\"` — \
                         WS-GAP-03 has 25 emit sites in this file, so a filter \
                         keyed on the code alone would page on ordinary \
                         connection churn"
                    )
                });

            // `code =` sits directly above `source =` in a tracing field list.
            // 200 bytes is one or two field lines, so this cannot drift into a
            // neighbouring emit and pass on someone else's code field.
            let above = &src[at.saturating_sub(200)..at];
            assert!(
                above.contains("ErrorCode::WsGapConnectionState.code_str()"),
                "the {label} source must sit on the SAME emit as the coded error \
                 a filter pairs it with — a source field on an uncoded line \
                 matches no three-condition filter"
            );
        }

        // Distinct values, not one shared label.
        assert_ne!(
            src.matches("source = \"xverify_vacuous\"").count(),
            0,
            "the vacuous label must be present"
        );
        assert_ne!(
            src.matches("source = \"xverify_failed\"").count(),
            0,
            "the failed label must be present"
        );
        // Exactly two REAL emissions. The needle also matches the `$.source =`
        // inside the filter-shape comment above the vacuous arm, so that one is
        // subtracted rather than the needle being narrowed — a narrower needle
        // (say, a fixed indentation prefix) would stop biting the moment
        // rustfmt moved the line, which is the failure mode this file's own
        // O(1) table records five separate times about line numbers.
        // 2026-08-25: the persistence wiring added two more emit sites, so
        // the assertion moved from "exactly two emissions" to "exactly this
        // SET of labels" — which is the property that actually matters and
        // does not have to be re-counted every time an arm is added.
        let mut labels: Vec<&str> = Vec::new();
        for (idx, _) in src.match_indices("source = \"xverify_") {
            let rest = &src[idx + "source = \"".len()..];
            if let Some(end) = rest.find('"') {
                labels.push(&rest[..end]);
            }
        }
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(
            labels,
            vec![
                "xverify_failed",
                "xverify_persist_partial",
                "xverify_vacuous"
            ],
            "the xverify source labels changed. Two of these are matched by CloudWatch \
             metric filters (`xverify_vacuous`, `xverify_failed`) and page; \
             `xverify_persist_partial` deliberately does not, because a partial write \
             still lands the daily verdict row and adding an alarm needs a dated \
             operator quote per dhan-rest-only-noise-lock-2026-07-14.md §3. A new label \
             here means a new failure mode that pages nobody — decide that deliberately."
        );

        // The TOTAL-loss persist arm must reuse the ALARMED label. A distinct
        // label would give better triage and reach no alarm, which is exactly
        // how the audit found a comment claiming persistence that never
        // happened: the failure would be invisible again.
        assert!(
            !src.contains("source = \"xverify_persist_failed\""),
            "a total persistence failure must page through the alarmed `xverify_failed` \
             label, not a private one nothing matches"
        );
    }

    #[test]
    fn test_pre_open_tick_is_written_even_though_it_opens_no_candle() {
        // The loss this closes: the candle session window is [09:15, 15:40)
        // IST, and a tick outside it used to exit through the SAME arm as a
        // NaN price — so the entire 09:00–09:15 pre-open produced no candle
        // (correct) AND no row in `ticks` (wrong). On a system whose stated
        // requirement is not missing a single tick, a CANDLE rule was silently
        // deciding what got CAPTURED.
        let mut ingest = LiveIngest::new(TickWriter::for_test(Feed::Dhan), 4);

        // 2026-08-14 is a Thursday; 03:10 UTC is 08:40 IST — inside the
        // pre-open, comfortably outside the candle session.
        let pre_open_ltt = 1_755_141_600_u32;
        let packet = ticker_packet(13, 23_146.45, pre_open_ltt);
        let parsed = dispatch_frame(&packet, i64::from(pre_open_ltt) * 1_000_000_000)
            .expect("a well-formed ticker packet must parse");
        let ParsedFrame::Tick(tick) = parsed else {
            panic!("response code 2 must dispatch to a Tick");
        };

        let outcome = ingest.ingest_tick(&tick, 7, u64::from(pre_open_ltt) * 1_000);

        // The distinction is the whole point: NOT `Folded` (no bucket opened,
        // and claiming otherwise would misreport the candle coverage) and NOT
        // `AggregatorRefused` (the row exists).
        assert!(
            matches!(outcome, IngestOutcome::WrittenOutOfSession),
            "a pre-open tick must be WRITTEN while opening no candle, got {outcome:?}"
        );
        assert_eq!(
            ingest.pending_rows(),
            1,
            "the pre-open tick must be buffered for the writer — this row IS the fix; \
             without it the pre-open window is captured nowhere"
        );
    }

    #[test]
    fn test_unusable_ticks_are_still_refused_outright() {
        // The other half of the same change, and the one that keeps it honest:
        // widening the out-of-session path must NOT widen the others. A tick
        // whose price is non-finite is corrupt data, and writing it would put
        // a garbage row in `ticks` — strictly worse than losing it, because a
        // lost tick is counted and a corrupt one is trusted.
        let mut ingest = LiveIngest::new(TickWriter::for_test(Feed::Dhan), 4);
        let packet = ticker_packet(13, f32::NAN, 1_779_355_000);
        let parsed = dispatch_frame(&packet, 1_779_355_000_000_000_000)
            .expect("the packet is well-formed; only its PRICE is not");
        let ParsedFrame::Tick(tick) = parsed else {
            panic!("response code 2 must dispatch to a Tick");
        };

        let outcome = ingest.ingest_tick(&tick, 9, 1_779_355_000_000);
        assert!(
            matches!(outcome, IngestOutcome::AggregatorRefused),
            "a non-finite price must be refused outright, got {outcome:?}"
        );
        assert_eq!(
            ingest.pending_rows(),
            0,
            "a corrupt tick must reach the writer buffer under NO circumstances"
        );
    }

    #[test]
    fn test_seal_open_buckets_at_close_accounts_every_bar_it_produces() {
        // The defect this pins: `force_seal_all` had ZERO production callers,
        // so the final open bucket of every timeframe was discarded at
        // shutdown — one bar per instrument per timeframe, lost daily, with
        // no counter moving and no log line.
        //
        // Honest scope: this unit test runs with no seal writer installed, so
        // every REQUESTED bar lands on the `dropped` side. That is deliberate
        // and it is still the assertion that matters — a non-zero total proves
        // the aggregator was actually walked and OPEN buckets were found,
        // which is precisely what the missing call site was failing to do. The
        // emitted-vs-dropped SPLIT is exercised by the writer-side tests; the
        // invariant here is that no bar escapes accounting on ANY side.
        //
        // Updated 2026-08-18 with the operator-timeframe gate: of the 24 bars
        // the fold produces, 13 are requested and reach dropped/emitted, and
        // 11 land in `seals_skipped`. This test FAILED when that gate first
        // shipped as a bare `return` — the eleven vanished from the ledger and
        // the production `debug_assert` caught it. That is the test doing
        // exactly its job, so the fix was a third counter, never a relaxed
        // assertion.
        let mut ingest = LiveIngest::new(TickWriter::for_test(Feed::Dhan), 4);
        let packet = ticker_packet(13, 23_146.45, 1_779_355_000);
        let ParsedFrame::Tick(tick) =
            dispatch_frame(&packet, 1_779_355_000_000_000_000).expect("parse")
        else {
            panic!("expected a tick");
        };
        ingest.ingest_tick(&tick, 1, 1_779_355_000_000);

        let before_emitted = ingest.seals_emitted();
        let before_dropped = ingest.seals_dropped();
        let (emitted, dropped) = ingest.seal_open_buckets_at_close();

        assert!(
            emitted.saturating_add(dropped) > 0,
            "one tick opens a bucket in every timeframe, so the close seal \
             must produce at least one bar — zero here means the aggregator \
             was never walked, which is the exact silent loss this exists to \
             prevent"
        );
        assert_eq!(
            ingest.seals_emitted().saturating_sub(before_emitted),
            emitted,
            "close-seal emissions must land in the SAME running counter as \
             the per-tick path — a close-time bar accounted separately is a \
             bar the operator cannot see"
        );
        assert_eq!(
            ingest.seals_dropped().saturating_sub(before_dropped),
            dropped,
            "close-time drops are as much a loss as mid-session drops and \
             must move the same counter"
        );

        // Idempotence: the buckets are consumed, so a second call at the same
        // shutdown cannot re-emit them. A double-seal would write duplicate
        // bars for the session's final minute.
        let (again_emitted, again_dropped) = ingest.seal_open_buckets_at_close();
        assert_eq!(
            (again_emitted, again_dropped),
            (0, 0),
            "sealing twice must not re-emit already-sealed buckets"
        );
    }

    #[test]
    fn test_flush_drains_the_buffer_so_rows_can_reach_the_database() {
        // The defect this pins: `append` only BUFFERS. Without a flush the
        // rows never leave the process, while the ingest counter happily
        // reports every one as folded — unbounded memory growth behind a green
        // light. `pending_rows` returning to 0 is the observable proof that a
        // flush was actually attempted.
        let mut ingest = LiveIngest::new(TickWriter::for_test(Feed::Dhan), 4);
        let packet = ticker_packet(13, 100.0, 1_779_355_000);
        let ParsedFrame::Tick(tick) =
            dispatch_frame(&packet, 1_779_355_000_000_000_000).expect("parse")
        else {
            panic!("expected a tick");
        };
        ingest.ingest_tick(&tick, 7, 1_779_355_000_000);
        assert_eq!(ingest.pending_rows(), 1, "precondition: one row buffered");

        ingest.flush();

        assert_eq!(
            ingest.pending_rows(),
            0,
            "flush must clear the buffer — rows left pending are rows that \
             never reached QuestDB"
        );
        // A second flush on an empty buffer is a no-op, not an error: the
        // 500 ms timer fires on idle instruments constantly.
        assert_eq!(ingest.flush(), 0, "flushing an empty buffer is a no-op");
    }

    /// A seeded-but-never-ticked instrument must be reported.
    ///
    /// This is the partial-subscribe detector's whole reason to exist: a
    // -----------------------------------------------------------------
    // Dead-instrument-class detector (Item 3)
    //
    // The unit tests below drive `ClassLiveness` directly because the
    // interesting cases are about the SHAPE of the tally, and a pure test can
    // state each one exactly. The end-to-end test that follows them proves
    // the fold is actually wired into the live sweep — without it, all of
    // these could pass against a struct nothing calls.
    // -----------------------------------------------------------------

    /// The 2026-08-21 incident, reduced to its essentials.
    ///
    /// Every index never ticked; every option did. The detector must name the
    /// index class and stay silent about the option class.
    #[test]
    fn a_class_where_nothing_ever_ticked_is_dead_and_a_live_class_beside_it_is_not() {
        let mut c = ClassLiveness::default();
        for _ in 0..119 {
            c.observe(ExchangeSegment::IdxI, false, true, true);
        }
        for _ in 0..8_868 {
            c.observe(ExchangeSegment::NseFno, false, false, true);
        }

        assert!(
            c.is_dead(segment_class_index(ExchangeSegment::IdxI)),
            "119 indices subscribed and not one tick past warmup is the \
             subscribe-did-not-take signature — this is the live incident the \
             per-instrument gauge showed as 119-of-9000 and nobody paged on"
        );
        assert!(
            !c.is_dead(segment_class_index(ExchangeSegment::NseFno)),
            "the option class was flowing normally and must never be swept up \
             with the dead one"
        );
    }

    /// A fully healthy lane must report NOTHING.
    ///
    /// # What this actually catches, measured
    ///
    /// Bite-tested rather than assumed, and the first version of this comment
    /// was WRONG about it. This test catches an INVERTED verdict (`never !=
    /// eligible`), which makes it fail immediately.
    ///
    /// It does NOT catch the `counts_toward_alarm()` denominator trap
    /// described on [`ClassLiveness`]. Under that mutation a healthy class has
    /// `eligible == 0`, so `eligible > 0` is false and this test still passes.
    /// The test that actually bites on that trap is
    /// `a_single_ticking_instrument_keeps_its_class_alive` — verified by
    /// mutating the denominator and watching exactly that one fail.
    ///
    /// Recorded because a comment claiming a test protects something it does
    /// not is the same false-OK this detector was written to end.
    #[test]
    fn a_healthy_class_is_never_reported_dead() {
        let mut c = ClassLiveness::default();
        for _ in 0..500 {
            c.observe(ExchangeSegment::NseEquity, false, false, true);
        }
        assert!(
            !c.is_dead(segment_class_index(ExchangeSegment::NseEquity)),
            "a class where every instrument has ticked is alive — reporting it \
             dead would train the operator to ignore this signal, which is \
             worse than not having it"
        );
    }

    /// No false page during warmup — the every-morning failure mode.
    #[test]
    fn a_class_still_inside_its_warmup_window_is_not_yet_judged() {
        let mut c = ClassLiveness::default();
        // Every instrument never-ticked, but none past its fair-chance window.
        for _ in 0..50 {
            c.observe(ExchangeSegment::IdxI, false, true, false);
        }
        assert!(
            !c.is_dead(segment_class_index(ExchangeSegment::IdxI)),
            "between subscribing and the first tick EVERY instrument is \
             legitimately never-ticked; judging here would declare every class \
             dead seconds after connect, every single morning"
        );

        // One straggler still warming is enough to withhold the verdict.
        let mut mixed = ClassLiveness::default();
        for _ in 0..49 {
            mixed.observe(ExchangeSegment::IdxI, false, true, true);
        }
        mixed.observe(ExchangeSegment::IdxI, false, true, false);
        assert!(
            !mixed.is_dead(segment_class_index(ExchangeSegment::IdxI)),
            "a single instrument still inside its window means the class has \
             not finished starting up — the verdict waits rather than guesses"
        );
    }

    /// Pins the invariant that makes `is_dead`'s `pending` term redundant.
    ///
    /// Found by bite-testing: mutating `pending == 0` away changed no verdict,
    /// because `eligible` counts every non-sparse instrument while `never`
    /// counts only past-window ones, so `pending > 0` already forces
    /// `never < eligible`. That reasoning is only sound while every counted
    /// instrument passes through `eligible` — this asserts exactly that, so a
    /// future fold that broke it fails here instead of silently demoting the
    /// warmup guard to decoration.
    #[test]
    fn the_tally_invariant_that_makes_the_pending_term_redundant_holds() {
        let mut c = ClassLiveness::default();
        c.observe(ExchangeSegment::IdxI, false, true, true); // never
        c.observe(ExchangeSegment::IdxI, false, true, false); // pending
        c.observe(ExchangeSegment::IdxI, false, false, true); // ticked
        c.observe(ExchangeSegment::IdxI, true, true, true); // sparse: counted nowhere

        let i = segment_class_index(ExchangeSegment::IdxI);
        assert_eq!(c.eligible[i], 3, "sparse must not reach any bucket");
        assert_eq!(c.never[i], 1);
        assert_eq!(c.pending[i], 1);
        assert!(
            c.never[i].saturating_add(c.pending[i]) <= c.eligible[i],
            "never + pending must never exceed eligible — the moment it can, \
             `pending == 0` stops being implied by the equality and the \
             documented redundancy argument becomes false"
        );

        // The implication itself, stated directly: a pending instrument makes
        // the dead-verdict equality unreachable on its own.
        assert!(
            c.never[i] < c.eligible[i],
            "with one instrument still warming, the equality cannot hold, \
             which is precisely why the explicit pending check never changes \
             a verdict"
        );
    }

    /// One live instrument keeps the whole class alive.
    ///
    /// # This is the test that guards the denominator trap
    ///
    /// `counts_toward_alarm()` is `!sparse && (Exceeded | NeverTicked)`, so it
    /// DROPS healthy instruments. Had the rollup reused it as the denominator,
    /// the one ticking instrument below would be filtered out, leaving
    /// `eligible == 99 == never` — and the class would read dead while it was
    /// demonstrably alive.
    ///
    /// Verified by mutation: making `eligible` count only never-ticked
    /// instruments fails THIS test and no other. That makes this the
    /// load-bearing guard on the single most dangerous mistake available in
    /// this fold, which is worth knowing before someone "simplifies" it.
    #[test]
    fn a_single_ticking_instrument_keeps_its_class_alive() {
        let mut c = ClassLiveness::default();
        for _ in 0..99 {
            c.observe(ExchangeSegment::BseFno, false, true, true);
        }
        c.observe(ExchangeSegment::BseFno, false, false, true);
        assert!(
            !c.is_dead(segment_class_index(ExchangeSegment::BseFno)),
            "this detector answers 'did the subscribe take for this segment', \
             and one tick proves it did — 99 quiet instruments are the \
             per-instrument gauge's business, not this one's"
        );
    }

    /// An instrument that ticked and then went quiet is NOT never-ticked.
    #[test]
    fn a_class_that_has_gone_quiet_is_not_a_class_that_never_started() {
        let mut c = ClassLiveness::default();
        // `Exceeded` instruments reach the fold as never_ticked = false.
        for _ in 0..30 {
            c.observe(ExchangeSegment::NseFno, false, false, true);
        }
        assert!(
            !c.is_dead(segment_class_index(ExchangeSegment::NseFno)),
            "gone-quiet and never-started have different causes and different \
             fixes; conflating them would make this signal unactionable"
        );
    }

    /// Sparse instruments leave BOTH sides of the ratio.
    ///
    /// This is the subtle half. Far-month futures and INDIA VIX are
    /// legitimately quiet and the scope lock excludes them from the silent
    /// count. Excluding them from the NUMERATOR only — while leaving them in
    /// the denominator — would make `never == eligible` unreachable for any
    /// segment containing one, and the detector would silently never fire.
    /// That is a false-OK, and it is invisible without this test.
    #[test]
    fn sparse_instruments_leave_both_sides_of_the_ratio() {
        let mut c = ClassLiveness::default();
        for _ in 0..10 {
            c.observe(ExchangeSegment::NseFno, false, true, true);
        }
        // Two legitimately-sparse contracts in the same segment.
        c.observe(ExchangeSegment::NseFno, true, true, true);
        c.observe(ExchangeSegment::NseFno, true, false, true);

        let i = segment_class_index(ExchangeSegment::NseFno);
        assert_eq!(
            c.eligible[i], 10,
            "sparse instruments must not inflate the denominator"
        );
        assert!(
            c.is_dead(i),
            "the ten judgeable instruments all produced nothing, so the class \
             is dead — if sparse entries had been left in the denominator this \
             would read alive and the detector would never fire at all"
        );

        // A segment made up ENTIRELY of sparse instruments is unjudgeable,
        // and must report nothing rather than guess either way.
        let mut all_sparse = ClassLiveness::default();
        all_sparse.observe(ExchangeSegment::BseFno, true, true, true);
        assert!(
            !all_sparse.is_dead(segment_class_index(ExchangeSegment::BseFno)),
            "with nothing judgeable there is no evidence, and absence of \
             evidence must not render as a verdict"
        );
    }

    /// An unseeded class is not a dead class.
    #[test]
    fn a_class_we_never_subscribed_is_not_reported_dead() {
        let c = ClassLiveness::default();
        for index in 0..SEGMENT_CLASS_COUNT {
            assert!(
                !c.is_dead(index),
                "an empty tally means we subscribed nothing in that segment; \
                 reporting it dead would page about instruments that do not \
                 exist"
            );
        }
    }

    /// The index mapping must be a bijection.
    ///
    /// If two segments ever collided on one slot their tallies would merge and
    /// a dead class could be masked by a live one sharing its index — a silent
    /// wrong answer with no other symptom.
    #[test]
    fn every_segment_maps_to_its_own_slot_and_back() {
        let all = [
            ExchangeSegment::IdxI,
            ExchangeSegment::NseEquity,
            ExchangeSegment::NseFno,
            ExchangeSegment::NseCurrency,
            ExchangeSegment::BseEquity,
            ExchangeSegment::McxComm,
            ExchangeSegment::BseCurrency,
            ExchangeSegment::BseFno,
        ];
        assert_eq!(all.len(), SEGMENT_CLASS_COUNT, "the array width must match");

        let mut seen = [false; SEGMENT_CLASS_COUNT];
        for segment in all {
            let i = segment_class_index(segment);
            assert!(!seen[i], "two segments collided on slot {i}");
            seen[i] = true;
            assert_eq!(
                segment_class_at(i),
                Some(segment),
                "index and inverse must agree, or an episode names the wrong \
                 class"
            );
        }
        assert!(seen.iter().all(|s| *s), "every slot must be claimed");
        assert_eq!(
            segment_class_at(SEGMENT_CLASS_COUNT),
            None,
            "out of range must be None, never a wrapped segment"
        );
    }

    /// END TO END: the fold is actually wired into the live sweep.
    ///
    /// Every unit test above would still pass if `ClassLiveness` were dead
    /// code nothing called. This one drives the real `LiveIngest` sweep, so it
    /// fails if the rollup is ever unhooked from `scan_silence_named`.
    #[test]
    fn the_dead_class_rollup_is_wired_into_the_live_silence_sweep() {
        let mut ingest = LiveIngest::new(TickWriter::for_test(Feed::Dhan), 8);
        let seeded_at = 1_000u64;
        for sid in [13u64, 25, 51] {
            assert!(
                ingest.seed(sid, ExchangeSegment::IdxI, seeded_at),
                "precondition: the index is tracked"
            );
        }

        let floor = tickvault_core::pipeline::tick_gap_detector::DEFAULT_SILENCE_FLOOR_MILLIS;

        // Inside the window: the class must not be judged yet, and the latch
        // must stay clear so a later real episode can still raise an edge.
        let _ = ingest.scan_silence(seeded_at);
        assert_eq!(
            ingest.dead_class_latch.load(Ordering::Relaxed),
            0,
            "no class may be latched while every instrument is still inside \
             its warmup window"
        );

        // Past the window with nothing received: the class is dead and the
        // latch records it exactly once.
        let _ = ingest.scan_silence(seeded_at + floor + 1);
        let bit = 1u8 << segment_class_index(ExchangeSegment::IdxI);
        assert_eq!(
            ingest.dead_class_latch.load(Ordering::Relaxed) & bit,
            bit,
            "three seeded indices past warmup with zero ticks is a dead class, \
             and the live sweep must be the thing that notices"
        );

        // Edge-latched: a second sweep in the same episode must not re-raise.
        let before = ingest.dead_class_latch.load(Ordering::Relaxed);
        let _ = ingest.scan_silence(seeded_at + floor + 2);
        assert_eq!(
            ingest.dead_class_latch.load(Ordering::Relaxed),
            before,
            "the sweep runs every 30s and a dead class stays dead all session; \
             re-reporting would emit ~1,100 identical lines per session and \
             bury the signal it exists to raise"
        );
    }

    /// subscribe that silently did not take produces no payload to count, no
    /// parse to fail and no error to log, so the ONLY evidence is absence
    /// measured against a key we know we asked for.
    #[test]
    fn test_scan_silence_reports_a_seeded_instrument_that_never_ticked() {
        let mut ingest = LiveIngest::new(TickWriter::for_test(Feed::Dhan), 4);
        let seeded_at = 1_000u64;
        assert!(
            ingest.seed(13, ExchangeSegment::IdxI, seeded_at),
            "precondition: the instrument is tracked"
        );
        assert_eq!(ingest.tracked_instruments(), 1);

        // Immediately after seeding, nothing may be counted yet.
        //
        // This assertion is the one that caught the defect. `classify_silence`
        // returns NeverTicked with NO elapsed-time condition, so the first
        // version of `scan_silence` reported (1, 1) here — meaning every
        // instrument in the book would have been counted as silent for the
        // whole gap between subscribing and its first tick, on every boot.
        let (silent, never) = ingest.scan_silence(seeded_at);
        assert_eq!(
            (silent, never),
            (0, 0),
            "an instrument seeded this instant has not had time to be silent — \
             counting it here is a false alarm on every single startup"
        );

        // Still inside the quiet ceiling: not yet evidence of anything.
        let floor = tickvault_core::pipeline::tick_gap_detector::DEFAULT_SILENCE_FLOOR_MILLIS;
        assert_eq!(
            ingest.scan_silence(seeded_at + floor),
            (0, 0),
            "at exactly the quiet ceiling the instrument is still given the \
             benefit of the doubt"
        );

        // Well past the detector's silence floor and it has still produced
        // nothing — that is the subscribe-did-not-take signature.
        let (silent, never) = ingest.scan_silence(
            seeded_at
                + tickvault_core::pipeline::tick_gap_detector::DEFAULT_SILENCE_FLOOR_MILLIS
                + 1,
        );
        assert_eq!(silent, 1, "a never-ticked instrument counts as silent");
        assert_eq!(
            never, 1,
            "and is reported SEPARATELY from merely-quiet ones, because the \
             cause is different: never-ticked usually means the subscribe \
             itself did not take"
        );
    }

    /// `tracked_instruments` counts what the detector actually holds.
    ///
    /// It exists so the silence page can say "3 of 4 quiet" rather than a
    /// bare "3" — the same number means very different things at a 4-SID
    /// universe and at the 25,000-instrument target, and an operator cannot
    /// triage the count without the denominator.
    #[test]
    fn test_tracked_instruments_counts_seeded_keys_and_ignores_repeats() {
        let mut ingest = LiveIngest::new(TickWriter::for_test(Feed::Dhan), 4);
        assert_eq!(
            ingest.tracked_instruments(),
            0,
            "a fresh book holds nothing"
        );

        assert!(ingest.seed(13, ExchangeSegment::IdxI, 1_000));
        assert!(ingest.seed(25, ExchangeSegment::IdxI, 1_000));
        assert_eq!(ingest.tracked_instruments(), 2);

        // Re-seeding a key already tracked must not double-count it — the
        // denominator would drift upward on every reconnect re-subscribe.
        assert!(ingest.seed(13, ExchangeSegment::IdxI, 2_000));
        assert_eq!(
            ingest.tracked_instruments(),
            2,
            "re-seeding an existing key must not inflate the count"
        );

        // I-P1-11: the same numeric id in a DIFFERENT segment is a different
        // instrument and must occupy its own slot.
        assert!(ingest.seed(13, ExchangeSegment::NseEquity, 1_000));
        assert_eq!(
            ingest.tracked_instruments(),
            3,
            "security_id alone is not unique — (id, segment) is"
        );
    }

    /// A refused tick must be COUNTED, because the refusal arm cannot log.
    ///
    /// The arm sits on the per-tick path where a log line would flood under a
    /// bad-data burst, so it stays silent and the 30s drain timer reports the
    /// delta instead. That only works if the count is actually kept — before
    /// this, `tv_dhan_feed_ingest_refused_total` incremented and reached
    /// nobody, and a tick refused for a bad price simply vanished while the
    /// lane reported healthy.
    #[test]
    fn test_aggregator_refusal_is_counted_for_the_periodic_report() {
        let mut ingest = LiveIngest::new(TickWriter::for_test(Feed::Dhan), 4);
        assert_eq!(
            ingest.refusals(),
            (0, 0, 0, 0),
            "a fresh fold has refused nothing"
        );

        // NaN LTP: refused by the aggregator's price sanity check.
        let packet = ticker_packet(13, f32::NAN, 1_779_355_000);
        let parsed = dispatch_frame(&packet, 1_779_355_000_000_000_000)
            .expect("a well-formed ticker packet must parse even with a NaN price");
        let ParsedFrame::Tick(tick) = parsed else {
            panic!("response code 2 must dispatch to a Tick");
        };

        let outcome = ingest.ingest_tick(&tick, 42, 1_779_355_000_000);
        assert!(
            matches!(outcome, IngestOutcome::AggregatorRefused),
            "a NaN price must be refused, got {outcome:?}"
        );

        let (price, ts, slot, oos) = ingest.refusals();
        assert_eq!(price, 1, "the price refusal must be counted for the report");
        assert_eq!((ts, slot), (0, 0), "only the price counter moves");
        assert_eq!(
            oos, 0,
            "an in-session tick must not be booked as out-of-session — that \
             bucket is deliberately excluded from the page"
        );
    }

    fn silent_at(id: u64, ms: u64) -> SilentInstrument {
        SilentInstrument {
            security_id: id,
            segment: ExchangeSegment::NseFno,
            silent_millis: ms,
            expected_millis: 1_000,
            never_ticked: false,
        }
    }

    /// The ranking keeps the QUIETEST, which is the whole point: at 25,000
    /// instruments only a handful can be named, so naming the wrong handful
    /// is the same as naming none.
    #[test]
    fn rank_silent_keeps_the_quietest_in_descending_order() {
        let mut buf = [SilentInstrument::EMPTY; WORST_SILENT_NAMED];
        let mut n = 0usize;
        for (id, ms) in [(1, 50), (2, 900), (3, 300), (4, 10)] {
            n = rank_silent(&mut buf, n, silent_at(id, ms));
        }
        assert_eq!(n, 4);
        assert_eq!(
            buf[..n].iter().map(|s| s.security_id).collect::<Vec<_>>(),
            vec![2, 3, 1, 4],
            "quietest first — the operator reads the top of the list"
        );
    }

    /// Past capacity it must DROP the least-quiet, not the newest arrival.
    /// Dropping by arrival order would make the named set depend on detector
    /// iteration order rather than on silence, which is arbitrary.
    #[test]
    fn rank_silent_evicts_the_least_silent_once_full() {
        let mut buf = [SilentInstrument::EMPTY; WORST_SILENT_NAMED];
        let mut n = 0usize;
        // Fill with an ascending run, so the buffer holds 8..1 descending.
        for id in 1..=WORST_SILENT_NAMED as u64 {
            n = rank_silent(&mut buf, n, silent_at(id, id * 10));
        }
        assert_eq!(n, WORST_SILENT_NAMED, "the buffer is full");

        // A new WORST arrival must land first and push the smallest out.
        n = rank_silent(&mut buf, n, silent_at(99, 10_000));
        assert_eq!(n, WORST_SILENT_NAMED, "the length never exceeds the cap");
        assert_eq!(buf[0].security_id, 99, "the quietest must lead");
        assert!(
            !buf.iter().any(|s| s.security_id == 1),
            "the least-silent entry is the one evicted"
        );

        // A new entry quieter than NOTHING in the buffer must be refused
        // rather than displacing a genuinely quieter one.
        n = rank_silent(&mut buf, n, silent_at(1_000, 1));
        assert!(
            !buf[..n].iter().any(|s| s.security_id == 1_000),
            "an entry that beats nothing in the buffer must not enter it"
        );
    }

    /// The named scan must agree with the counting scan, or the page's count
    /// and its names describe different things.
    #[test]
    fn scan_silence_named_agrees_with_the_counting_scan() {
        let mut ingest = LiveIngest::new(TickWriter::for_test(Feed::Dhan), 8);
        let mut buf = [SilentInstrument::EMPTY; WORST_SILENT_NAMED];

        let (silent_a, never_a, named_a) = ingest.scan_silence_named(u64::MAX, &mut buf);
        assert_eq!(
            (silent_a, never_a),
            ingest.scan_silence(u64::MAX),
            "the two entry points must report the same counts"
        );
        assert_eq!(
            named_a, 0,
            "an empty book names nobody — reporting a name here would page on \
             every boot before the first subscribe"
        );

        // Seed one and let it cross the silence floor: it must now be NAMED,
        // not merely counted, which is the defect this whole path fixes.
        ingest.seed(13, ExchangeSegment::IdxI, 0);
        let (silent_b, _, named_b) = ingest.scan_silence_named(u64::MAX, &mut buf);
        assert!(silent_b >= 1, "a long-quiet seeded instrument counts");
        assert!(named_b >= 1, "and it must be NAMED, not just counted");
        assert_eq!(
            buf[0].security_id, 13,
            "the name must be the real security_id — a count with no id is \
             exactly what made this unanswerable before"
        );
        assert!(
            buf[0].never_ticked,
            "an instrument that produced nothing since being seeded must say so \
             — that is the shape of a subscribe that did not take"
        );
    }

    /// The detector's own blindness, and the reason it needs an accessor.
    ///
    /// `scan_silence` reports how many tracked instruments are quiet. It
    /// cannot report the ones it never accepted, and past capacity that is a
    /// growing set — so a full detector returns a small, calm number that
    /// reads exactly like health. This accessor is the only way a caller can
    /// tell "nothing is silent" from "I can no longer see".
    /// The detector must be sized for the universe the lane will EVER carry,
    /// not for the one that exists at boot.
    ///
    /// MEASURED live 2026-08-25: `refused: 1,276,658` against `tracked: 865`.
    /// 865 is exactly the spot universe — the ~22,000 contracts attach minutes
    /// after boot, and every one of their ticks was refused a detector slot,
    /// so the silence counts described 4% of the subscribed set while reading
    /// as though they described all of it.
    #[test]
    fn the_detector_outlives_the_boot_time_universe_when_given_the_ceiling() {
        // Boot-time pre-size of 2 — the shape `new` is handed at construction.
        let mut ingest = LiveIngest::new(TickWriter::for_test(Feed::Dhan), 2)
            .with_detector_capacity(super::AGGREGATOR_MAX_SLOTS);

        // Seed far more instruments than the boot-time figure: this is the
        // late-attach growth that used to hit the hard cap.
        let mut seeded = 0usize;
        for sid in 0..5_000u64 {
            if ingest.seed(sid, ExchangeSegment::NseFno, 1_000) {
                seeded += 1;
            }
        }

        assert_eq!(
            seeded, 5_000,
            "every instrument past the boot-time pre-size must still get a slot"
        );
        assert_eq!(
            ingest.detector_refused(),
            0,
            "a detector sized at the ceiling turns nobody away below the ceiling"
        );
        assert_eq!(ingest.tracked_instruments(), 5_000);
    }

    /// The ceiling is still a ceiling — this is a re-size, not an uncapping.
    ///
    /// Fail-closed is the deliberate shape here (evicting a tracked instrument
    /// would silently reset its gap state and hide the next gap), so past the
    /// bound it must still refuse and still count.
    #[test]
    fn a_detector_at_its_ceiling_still_refuses_and_still_counts() {
        let mut ingest =
            LiveIngest::new(TickWriter::for_test(Feed::Dhan), 1).with_detector_capacity(3);

        for sid in 0..3u64 {
            assert!(ingest.seed(sid, ExchangeSegment::NseFno, 1_000));
        }
        assert_eq!(
            ingest.detector_refused(),
            0,
            "nothing refused under the cap"
        );

        assert!(
            !ingest.seed(99, ExchangeSegment::NseFno, 1_000),
            "the instrument past the cap must be refused, not silently accepted"
        );
        assert_eq!(
            ingest.detector_refused(),
            1,
            "and the refusal must be counted — a blind detector that says nothing \
             is the failure this whole path exists to make visible"
        );
        assert_eq!(
            ingest.tracked_instruments(),
            3,
            "a tracked instrument is never evicted to make room"
        );
    }

    /// The boot site must hand the detector the CEILING, not the boot-time
    /// count — the fix is worthless if only the builder exists.
    #[test]
    fn the_production_boot_site_sizes_the_detector_at_the_authorized_ceiling() {
        let full = include_str!("dhan_feed_stack.rs");
        let test_marker = concat!("#[cfg(", "test)]");
        let src = full.split(test_marker).next().unwrap_or(full);

        assert_eq!(
            src.matches(".with_detector_capacity(").count(),
            1,
            "exactly one production call site — a second would make the detector's size \
             depend on which one ran last"
        );
        assert!(
            src.contains(".with_detector_capacity(AGGREGATOR_MAX_SLOTS)"),
            "the detector must be sized at the authorized ceiling, never at the boot-time \
             universe: `capacity` is computed before any socket opens, so it counts the spot \
             set and misses every contract that attaches later"
        );
    }

    #[test]
    fn detector_refused_separates_a_quiet_universe_from_a_blind_detector() {
        let mut ingest = LiveIngest::new(TickWriter::for_test(Feed::Dhan), 2);
        assert_eq!(
            ingest.detector_refused(),
            0,
            "a fresh detector has turned nobody away"
        );

        // Fill the slot table exactly.
        assert!(ingest.seed(13, ExchangeSegment::IdxI, 0));
        assert!(ingest.seed(25, ExchangeSegment::IdxI, 0));
        assert_eq!(ingest.tracked_instruments(), 2);
        assert_eq!(
            ingest.detector_refused(),
            0,
            "seeding up to capacity refuses nobody"
        );

        // One past it. The seed is refused fail-closed, which is correct —
        // what matters is that the refusal is now VISIBLE.
        assert!(
            !ingest.seed(51, ExchangeSegment::IdxI, 0),
            "past capacity the allocator must refuse rather than grow"
        );
        assert!(
            ingest.detector_refused() >= 1,
            "the refusal must be counted, or the detector goes blind silently \
             and its silent-instrument count keeps reading like health"
        );

        // The trap this guards: the visible counts stay calm while the
        // detector is blind to instrument 51 entirely.
        assert_eq!(
            ingest.tracked_instruments(),
            2,
            "the refused instrument is genuinely absent from the book, which \
             is exactly why nothing else can report it"
        );
    }

    /// An empty book is not a silent book.
    #[test]
    fn test_scan_silence_on_an_empty_book_reports_nothing() {
        let ingest = LiveIngest::new(TickWriter::for_test(Feed::Dhan), 4);
        assert_eq!(ingest.tracked_instruments(), 0);
        assert_eq!(
            ingest.scan_silence(u64::MAX),
            (0, 0),
            "with nothing subscribed there is nothing to be silent — reporting \
             a count here would page on every boot before the first subscribe"
        );
    }

    /// An instrument that IS ticking must not be reported.
    #[test]
    fn test_scan_silence_does_not_report_a_live_instrument() {
        let mut ingest = LiveIngest::new(TickWriter::for_test(Feed::Dhan), 4);
        let ts = 1_779_355_000u32;
        let packet = ticker_packet(13, 100.0, ts);
        let ParsedFrame::Tick(tick) =
            dispatch_frame(&packet, i64::from(ts) * 1_000_000_000).expect("parse")
        else {
            panic!("expected a tick");
        };
        let recv_millis = u64::from(ts) * 1_000;
        ingest.ingest_tick(&tick, 7, recv_millis);

        let (silent, never) = ingest.scan_silence(recv_millis);
        assert_eq!(
            (silent, never),
            (0, 0),
            "an instrument that just ticked is live — a detector that reports \
             it would be crying wolf on the healthy case, which is how these \
             alerts get muted"
        );
    }

    /// The catch-up seal must never close a bucket the watermark has not
    /// cleared by the full lateness margin.
    ///
    /// This is the failure mode that makes the fix worse than the problem: a
    /// bar sealed while its own last ticks are still in flight is TRUNCATED —
    /// wrong data written confidently — whereas the defect being fixed is
    /// merely a correct bar arriving late.
    #[test]
    fn test_catch_up_seal_never_seals_inside_the_lateness_margin() {
        let mut ingest = LiveIngest::new(TickWriter::for_test(Feed::Dhan), 4);
        let ts = 1_779_355_000;
        let packet = ticker_packet(13, 100.0, ts);
        let ParsedFrame::Tick(tick) =
            dispatch_frame(&packet, i64::from(ts) * 1_000_000_000).expect("parse")
        else {
            panic!("expected a tick");
        };
        ingest.ingest_tick(&tick, 7, u64::from(ts) * 1_000);

        // The watermark now sits at `ts`, so the cutoff is `ts - margin`. The
        // bucket this tick opened ends AFTER that cutoff, so nothing may seal.
        let (emitted, dropped) = ingest.catch_up_seal();
        assert_eq!(
            (emitted, dropped),
            (0, 0),
            "a bucket whose end is inside the lateness margin must stay open — \
             sealing it would write a truncated bar, which is worse than the \
             late bar this mechanism exists to prevent"
        );
    }

    /// A watermark below the margin (session not yet started) must be a
    /// no-op, not an underflow into a huge cutoff that seals everything.
    #[test]
    fn test_catch_up_seal_is_a_no_op_before_the_watermark_moves() {
        let mut ingest = LiveIngest::new(TickWriter::for_test(Feed::Dhan), 4);
        assert_eq!(
            ingest.catch_up_seal(),
            (0, 0),
            "with a zero watermark the saturating cutoff is 0, which must seal \
             nothing — an underflow here would wrap to ~u32::MAX and seal every \
             open bucket in the book at once"
        );
    }

    /// The catch-up seal is wired into the drain loop and does not ride the
    /// flush timer.
    ///
    /// Ratchet for the defect: `catch_up_seal_all` existed, was documented,
    /// was tested, and had ZERO production callers — so every bar for an
    /// instrument that stopped ticking waited for the 15:30 close sweep.
    #[test]
    fn test_the_drain_runs_the_catch_up_seal() {
        let src = include_str!("dhan_feed_stack.rs");
        let body = src
            .split_once("async fn run_frame_drain")
            .expect("the drain function must exist")
            .1;
        for needle in [
            "let mut catchup_timer",
            "catchup_timer.tick()",
            "ingest.catch_up_seal()",
        ] {
            assert!(
                body.contains(needle),
                "run_frame_drain must contain `{needle}` — without it a bar for an \
                 instrument that stops ticking mid-session is not written until the \
                 session-close sweep"
            );
        }
        assert!(
            CATCHUP_LATENESS_MARGIN_SECS >= 1,
            "a zero margin would seal at the watermark itself, truncating bars \
             whose final ticks are still in flight"
        );
    }

    #[test]
    fn test_ingest_tick_at_yields_a_distinct_sequence_per_packet() {
        // One WebSocket message, two packets for the SAME instrument. If both
        // rows shared the frame's single sequence they would share every
        // column of the ticks DEDUP key and QuestDB would upsert one onto the
        // other — one tick silently gone, both counted as folded.
        let mut ingest = LiveIngest::new(TickWriter::for_test(Feed::Dhan), 4);
        let packet = ticker_packet(13, 100.0, 1_779_355_000);
        let ParsedFrame::Tick(tick) =
            dispatch_frame(&packet, 1_779_355_000_000_000_000).expect("parse")
        else {
            panic!("expected a tick");
        };

        assert!(matches!(
            ingest.ingest_tick_at(&tick, 1_000, 0, 1_779_355_000_000),
            IngestOutcome::Folded { .. }
        ));
        assert!(matches!(
            ingest.ingest_tick_at(&tick, 1_000, 1, 1_779_355_000_000),
            IngestOutcome::Folded { .. }
        ));
        assert_eq!(
            ingest.pending_rows(),
            2,
            "both packets must produce their own row"
        );
    }

    #[test]
    fn test_capture_seq_from_frame_seq_narrows_or_refuses() {
        // Narrowing must never saturate. Saturating would pin every later tick
        // to `i64::MAX`, collapsing them all onto ONE row under the DEDUP key —
        // unbounded silent loss. Refusing loses one tick, loudly.
        assert_eq!(capture_seq_from_frame_seq(0), Some(0));
        assert_eq!(capture_seq_from_frame_seq(42), Some(42));
        let max_ok = u64::try_from(i64::MAX).expect("i64::MAX fits u64");
        assert_eq!(capture_seq_from_frame_seq(max_ok), Some(i64::MAX));
        assert_eq!(
            capture_seq_from_frame_seq(max_ok + 1),
            None,
            "an unrepresentable sequence must be REFUSED, never clamped"
        );
        assert_eq!(capture_seq_from_frame_seq(u64::MAX), None);
    }

    #[test]
    fn test_seq_unrepresentable_tick_is_refused_and_counted_not_written() {
        // The fail-closed path: nothing folded, nothing buffered, and the
        // refusal is visible. A silently-stamped tick would corrupt a row that
        // already exists.
        let mut ingest = LiveIngest::new(TickWriter::for_test(Feed::Dhan), 4);
        let packet = ticker_packet(13, 100.0, 1_779_355_000);
        let ParsedFrame::Tick(tick) =
            dispatch_frame(&packet, 1_779_355_000_000_000_000).expect("parse")
        else {
            panic!("expected a tick");
        };

        let outcome = ingest.ingest_tick(&tick, u64::MAX, 1_779_355_000_000);

        assert_eq!(outcome, IngestOutcome::SeqUnrepresentable);
        assert_eq!(ingest.seq_refused(), 1, "the refusal must be counted");
        assert_eq!(
            ingest.pending_rows(),
            0,
            "a refused tick must not reach the writer buffer"
        );
    }

    #[test]
    fn test_crossverify_targets_and_crossverify_deps_installed_mirror_reality() {
        // The comparator must verify exactly what was captured. Verifying a
        // different set would produce a clean verdict about instruments the
        // lane never subscribed.
        let universe = hardcoded_index_universe();
        let (targets, skipped) = crossverify_targets_with_skipped(&universe);

        assert_eq!(
            targets.len(),
            universe.len(),
            "one target per subscribed instrument, no more and no fewer"
        );
        assert_eq!(skipped, 0, "every index is targetable");
        for (t, i) in targets.iter().zip(universe.iter()) {
            assert_eq!(t.security_id, i64::try_from(i.security_id).expect("fits"));
            assert_eq!(t.segment, i.segment.as_str());
        }
        assert!(
            crossverify_targets(&[]).is_empty(),
            "an empty universe yields no targets rather than a default one"
        );
    }

    /// BITE TEST (2026-08-25) — the partial-denominator vacuous pass.
    ///
    /// `instrument` used to be the literal `"INDEX"` for every target, and it
    /// goes verbatim into the Dhan REST intraday body. The live universe is
    /// ~119 indices plus ~750 NSE_EQ constituents, so ~86% of every run's
    /// fetches asked for a STOCK as though it were an INDEX — returning no
    /// candles, landing in `rest_failures`, and never being compared, while the
    /// run could still report `Clean` on the correctly-labelled indices.
    ///
    /// The module's `minutes_compared > 0` guard cannot catch this: the
    /// denominator is partial, not zero.
    #[test]
    fn an_equity_is_never_targeted_as_an_index_and_fno_is_never_guessed() {
        let universe = vec![
            SubscribeInstrument {
                security_id: 13,
                segment: ExchangeSegment::IdxI,
            },
            SubscribeInstrument {
                security_id: 2885,
                segment: ExchangeSegment::NseEquity,
            },
            SubscribeInstrument {
                security_id: 500_325,
                segment: ExchangeSegment::BseEquity,
            },
            SubscribeInstrument {
                security_id: 45_800,
                segment: ExchangeSegment::NseFno,
            },
        ];
        let (targets, skipped) = crossverify_targets_with_skipped(&universe);

        assert_eq!(
            targets.len(),
            3,
            "the three cash instruments are targetable"
        );
        assert_eq!(
            skipped, 1,
            "the F&O contract is COUNTED as unverifiable, never guessed"
        );
        assert_eq!(targets[0].instrument, "INDEX");
        assert_eq!(
            targets[1].instrument, "EQUITY",
            "an NSE_EQ constituent fetched as INDEX returns nothing and is \
             silently never compared"
        );
        assert_eq!(targets[2].instrument, "EQUITY");
        assert!(
            targets.iter().all(|t| t.security_id != 0),
            "an out-of-range id must be skipped, never coerced to instrument 0"
        );
    }

    #[test]
    fn test_crossverify_labels_an_equity_as_equity_not_index() {
        // The bite test for the 2026-08-25 fix. Today's universe is ~119 NSE
        // indices plus ~750 NTM equities; the previous code stamped "INDEX" on
        // every one, so six of every seven targets asked Dhan for an index bar
        // on an equity id. Dhan answers that pair with an empty candle set, so
        // the comparator counted it `missing_rest` — an absent vendor tape
        // reported where the real fault was our own request.
        //
        // Restore `instrument: "INDEX".to_string()` in `crossverify_targets`
        // and this assertion fails with left: "INDEX", right: "EQUITY".
        let universe = vec![
            SubscribeInstrument {
                security_id: 13,
                segment: ExchangeSegment::IdxI,
            },
            SubscribeInstrument {
                security_id: 2885,
                segment: ExchangeSegment::NseEquity,
            },
            SubscribeInstrument {
                security_id: 500_325,
                segment: ExchangeSegment::BseEquity,
            },
        ];
        let targets = crossverify_targets(&universe);
        assert_eq!(targets.len(), 3, "every cash segment is labellable");
        assert_eq!(targets[0].instrument, "INDEX");
        assert_eq!(targets[1].instrument, "EQUITY");
        assert_eq!(targets[2].instrument, "EQUITY");
        // The segment string must keep travelling verbatim: the pair is what
        // Dhan validates, so a right label on a wrong segment is no better.
        assert_eq!(targets[1].segment, "NSE_EQ");
        assert_eq!(targets[2].segment, "BSE_EQ");
    }

    #[test]
    fn test_crossverify_drops_a_segment_it_cannot_label_rather_than_guessing() {
        // An F&O id may be FUTIDX, OPTIDX, FUTSTK or OPTSTK and
        // `SubscribeInstrument` carries only (security_id, segment), so no
        // label here can be honest. Dropping it leaves the target UNVERIFIED
        // and says so; guessing would leave it verified-against-nothing, which
        // reads identically to a clean run.
        let universe = vec![
            SubscribeInstrument {
                security_id: 13,
                segment: ExchangeSegment::IdxI,
            },
            SubscribeInstrument {
                security_id: 45_678,
                segment: ExchangeSegment::NseFno,
            },
            SubscribeInstrument {
                security_id: 84_321,
                segment: ExchangeSegment::BseFno,
            },
        ];
        let targets = crossverify_targets(&universe);
        assert_eq!(
            targets.len(),
            1,
            "only the index survives; the two contracts are excluded, not mislabelled"
        );
        assert_eq!(targets[0].segment, "IDX_I");
        assert!(
            targets
                .iter()
                .all(|t| t.instrument != "INDEX" || t.segment == "IDX_I"),
            "no surviving target may carry INDEX on a non-index segment"
        );
    }

    #[test]
    fn test_dhan_intraday_instrument_for_covers_every_variant_deliberately() {
        // Pins the mapping so a new segment cannot default into a label. Each
        // arm below is a decision, not an accident.
        assert_eq!(
            dhan_intraday_instrument_for(ExchangeSegment::IdxI),
            Some("INDEX")
        );
        assert_eq!(
            dhan_intraday_instrument_for(ExchangeSegment::NseEquity),
            Some("EQUITY")
        );
        assert_eq!(
            dhan_intraday_instrument_for(ExchangeSegment::BseEquity),
            Some("EQUITY")
        );
        for ambiguous in [
            ExchangeSegment::NseFno,
            ExchangeSegment::BseFno,
            ExchangeSegment::NseCurrency,
            ExchangeSegment::BseCurrency,
            ExchangeSegment::McxComm,
        ] {
            assert_eq!(
                dhan_intraday_instrument_for(ambiguous),
                None,
                "{} must refuse a label rather than invent one",
                ambiguous.as_str()
            );
        }
    }

    #[test]
    fn test_spawn_daily_crossverify_refuses_unless_install_crossverify_deps_ran() {
        // A live lane with no verifier cannot detect the packet loss its
        // protocol cannot report. `None` is a refusal, not a skip — and the
        // caller logs it at ERROR.
        //
        // Note this asserts the state of a process-global OnceLock: in a test
        // binary nothing installs deps, so `installed` is false here.
        assert!(
            !crossverify_deps_installed(),
            "no test may install the global provider — it would leak across tests"
        );
        assert!(
            spawn_daily_crossverify(&hardcoded_index_universe()).is_none(),
            "without a provider the comparator must refuse to spawn"
        );
    }

    #[test]
    fn test_crossverify_schedule_lands_on_1531_ist_and_never_double_fires() {
        // One minute after the close, so the final minute has sealed.
        //
        // RE-BLESSED 2026-08-25 from a hardcoded 55_860 (15:31). That literal
        // was correct for the pre-CAS 15:30 close and became wrong on
        // 2026-08-07 when the NSE CAS migration moved the session end to 15:40
        // everywhere except here and the comparator's own window constant. The
        // schedule is now DERIVED from the close, and the relationship — not a
        // literal — is what this test pins, so the next session-hours change
        // cannot leave it behind a seventh time.
        const RUN: u64 = XVERIFY_RUN_AT_SECS_OF_DAY_IST;
        assert_eq!(
            RUN as i64,
            crate::dhan_live_crossverify::SESSION_CLOSE_SECS_OF_DAY_IST + 60,
            "the comparator must fire exactly one minute after the session close"
        );
        assert_eq!(RUN, 56_460, "09:15-15:40 session ⇒ a 15:41 IST run");

        // Before the run time: wait until today's.
        assert_eq!(secs_until_next_run_ist(0), RUN, "midnight → today's run");
        assert_eq!(
            secs_until_next_run_ist(RUN - 1),
            1,
            "one second before → one second to wait"
        );

        // AT the run time: a full day, never zero. Zero would busy-loop the
        // task and fire the comparator repeatedly within one session.
        assert_eq!(
            secs_until_next_run_ist(RUN),
            SECS_PER_DAY,
            "exactly at the run time must wait a full day, not fire again"
        );

        // After: tomorrow's.
        assert_eq!(secs_until_next_run_ist(RUN + 1), SECS_PER_DAY - 1);
        assert_eq!(
            secs_until_next_run_ist(SECS_PER_DAY - 1),
            RUN + 1,
            "one second before midnight → tomorrow's run"
        );

        // Total over every second of the day: always a positive, bounded wait.
        for s in (0..SECS_PER_DAY).step_by(97) {
            let wait = secs_until_next_run_ist(s);
            assert!(
                wait > 0 && wait <= SECS_PER_DAY,
                "wait from {s} was {wait} — must be positive and at most one day"
            );
        }
    }

    #[test]
    fn test_now_ist_secs_of_day_is_within_a_day() {
        // Total by construction; pinned so a timezone-handling change cannot
        // silently produce an out-of-range value that skews the schedule.
        assert!(now_ist_secs_of_day() < SECS_PER_DAY);
    }

    #[test]
    fn test_packet_len_is_known_for_every_dispatchable_code_and_rejects_junk() {
        // The frame walker steps by this table. A wrong length walks into the
        // middle of the next packet and fabricates ticks from misaligned
        // bytes, so it is pinned per code rather than trusted to the
        // vendor-supplied length field in the header.
        // Code 1 (INDEX ticker) shares the 16-byte ticker layout and is
        // accepted by `dispatch_frame`; it was absent from BOTH this list and
        // the table until 2026-08-26, so the test agreed with the bug. The
        // exhaustive sweep in `parser::dispatcher`'s own tests is the real
        // guard now — this stays as the readable smoke check.
        assert_eq!(main_feed_packet_len(&[1]), Some(TICKER_PACKET_SIZE));
        assert_eq!(main_feed_packet_len(&[2]), Some(TICKER_PACKET_SIZE));
        assert_eq!(main_feed_packet_len(&[4]), Some(QUOTE_PACKET_SIZE));
        assert_eq!(main_feed_packet_len(&[5]), Some(OI_PACKET_SIZE));
        assert_eq!(main_feed_packet_len(&[6]), Some(PREVIOUS_CLOSE_PACKET_SIZE));
        assert!(main_feed_packet_len(&[7]).is_some());
        assert_eq!(main_feed_packet_len(&[8]), Some(FULL_QUOTE_PACKET_SIZE));
        assert_eq!(main_feed_packet_len(&[50]), Some(DISCONNECT_PACKET_SIZE));
        // Unknown code and empty input: `None`, so the walker stops rather
        // than resynchronising on a guess.
        assert_eq!(main_feed_packet_len(&[99]), None);
        assert_eq!(main_feed_packet_len(&[]), None);
    }

    #[tokio::test]
    async fn test_frame_drain_folds_then_ends_when_every_sender_is_dropped() {
        // The socket→fold edge with no network: a ticker frame in, a folded
        // row out, and a clean end once the last sender is gone. The final
        // flush on the way out is what saves the tail of a session.
        let ingest = LiveIngest::new(TickWriter::for_test(Feed::Dhan), 4);
        let (tx, rx) = tokio::sync::mpsc::channel::<CapturedFrame>(4);

        tx.send(CapturedFrame {
            seq: 42,
            endpoint: DhanEndpointType::MainFeed,
            connection_index: 0,
            received_at: std::time::Instant::now(),
            bytes: bytes::Bytes::copy_from_slice(&ticker_packet(13, 23_146.45, 1_779_355_000)),
        })
        .await
        .expect("the ring must accept a frame");
        drop(tx);

        // Completes rather than hanging: the drain must exit when its last
        // sender is gone, or a dead lane would look alive forever.
        let drained = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            run_frame_drain(
                rx,
                ingest,
                None,
                Arc::new(RingByteBudget::new(MAIN_FEED_RING_MAX_BYTES)),
                Arc::new(RingByteBudget::new(DEPTH_RING_MAX_BYTES)),
                Arc::new(tokio::sync::Notify::new()),
                Arc::new(tickvault_common::feed_health::FeedHealthRegistry::new()),
                tokio::sync::mpsc::channel(1).1,
            ),
        )
        .await
        .expect("the drain must end when the ring closes");

        // And it must actually have FOLDED the frame. Until 2026-08-11 this
        // test stopped at the line above — it asserted only that the future
        // completed, which a drain that discarded every frame would satisfy
        // just as well. The name said "folds"; the body checked "ends".
        assert_eq!(
            drained.folded, 1,
            "the ticker frame must be folded, not merely consumed"
        );
        assert_eq!(
            drained.ingest.pending_rows(),
            0,
            "the drain's exit flush must clear the buffer — this is the tail of the \
             session, and rows left pending here never reach the database"
        );
        assert_eq!(
            drained.unparseable, 0,
            "a well-formed ticker frame must not be counted unparseable"
        );
        // (see `test_drain_exits_on_shutdown_signal_with_the_ring_still_open`
        // for the other exit path — the one that did not exist until
        // 2026-08-14.)
    }

    /// The drain must exit on the shutdown signal **while the ring is still
    /// open** — the exit path that did not exist until 2026-08-14.
    ///
    /// Holding a live sender is the whole point of this test. Before the
    /// shutdown arm, `rx.recv()` was the only way out, so a drain with any
    /// sender alive ran forever. At the 17:30 stop nothing closed the sockets,
    /// so nothing closed the ring, so the final `ingest.flush()` never ran and
    /// the day's tail died with the process — while the log printed
    /// "tickvault stopped" and classified the shutdown clean.
    ///
    /// The timeout is the assertion: a drain that ignores the signal hangs
    /// here rather than failing an equality check, which is the honest shape
    /// for "this must terminate".
    #[tokio::test]
    async fn test_drain_exits_on_shutdown_signal_with_the_ring_still_open() {
        let ingest = LiveIngest::new(TickWriter::for_test(Feed::Dhan), 4);
        let (tx, rx) = tokio::sync::mpsc::channel::<CapturedFrame>(4);
        let shutdown = Arc::new(tokio::sync::Notify::new());

        tx.send(CapturedFrame {
            seq: 7,
            endpoint: DhanEndpointType::MainFeed,
            connection_index: 0,
            received_at: std::time::Instant::now(),
            bytes: bytes::Bytes::copy_from_slice(&ticker_packet(13, 23_146.45, 1_779_355_000)),
        })
        .await
        .expect("the ring must accept a frame");

        let drain = tokio::spawn(run_frame_drain(
            rx,
            ingest,
            None,
            Arc::new(RingByteBudget::new(MAIN_FEED_RING_MAX_BYTES)),
            Arc::new(RingByteBudget::new(DEPTH_RING_MAX_BYTES)),
            Arc::clone(&shutdown),
            Arc::new(tickvault_common::feed_health::FeedHealthRegistry::new()),
            tokio::sync::mpsc::channel(1).1,
        ));

        // `notify_one` is permit-based, so this is safe to fire before the
        // drain has parked on its shutdown arm — the permit is retained. That
        // is precisely why it is used instead of `notify_waiters`, whose wake
        // would be lost if the drain happened to be inside another arm.
        shutdown.notify_one();

        let outcome = tokio::time::timeout(std::time::Duration::from_secs(5), drain)
            .await
            .expect(
                "the drain must exit on the shutdown signal even with a live sender still \
                 holding the ring open — hanging here IS the bug this test exists to catch",
            )
            .expect("the drain task must not panic");

        assert_eq!(
            outcome.folded, 1,
            "the drain should have folded the queued frame before honouring shutdown — \
             exiting on the signal must not mean abandoning what was already in the ring"
        );
        assert_eq!(
            outcome.ingest.pending_rows(),
            0,
            "the shutdown path must FLUSH, not merely exit. Rows left pending here are \
             exactly the day's tail that used to die with the process every evening."
        );

        // The sender is still alive until this line, proving the exit came
        // from the signal and not from the ring closing.
        drop(tx);
    }

    #[test]
    fn test_an_idle_flush_cannot_forge_feed_liveness() {
        // `flush_and_record` runs on the 500 ms timer whether or not anything
        // ticked, so the ZERO case is the one that decides whether this
        // instrumentation is honest or actively harmful. If a no-row flush
        // stamped the clock, an instrument set that never ticked would look
        // permanently fresh — the Dhan verdict would move from "can never say
        // Down" (the bug being fixed) to "says Up while dead", which is
        // strictly worse than the state it replaced.
        //
        // The guarantee comes from `record_ticks`'s `n == 0` early return, so
        // this test pins the BEHAVIOUR that `flush_and_record` relies on
        // rather than restating its implementation.
        let reg = tickvault_common::feed_health::FeedHealthRegistry::new();
        let now = 1_779_355_000_000_000_000_i64;

        reg.record_ticks(Feed::Dhan, 0, now);
        assert_eq!(
            reg.last_tick_age_secs(Feed::Dhan, now),
            None,
            "a flush that covered ZERO rows must not stamp the clock — an idle \
             500 ms timer tick would otherwise report a dead feed as fresh"
        );

        // And the positive case, so the assertion above cannot pass simply
        // because nothing works.
        reg.record_ticks(Feed::Dhan, 3, now);
        assert_eq!(
            reg.last_tick_age_secs(Feed::Dhan, now),
            Some(0),
            "a flush that covered rows MUST stamp the clock — without this the \
             Dhan verdict can never leave `Unknown`, which is the whole defect"
        );
    }

    #[tokio::test]
    async fn test_depth_frames_are_not_fed_to_the_main_feed_parser() {
        // Depth uses a 12-byte header whose first bytes are a message length,
        // so byte 0 matches no main-feed response code. Before endpoint
        // routing existed, every depth frame was handed to the main-feed
        // dispatcher and counted "unparseable" — a 100% silent loss of a
        // surface the operator explicitly authorised. The drain must survive
        // one and terminate cleanly.
        let ingest = LiveIngest::new(TickWriter::for_test(Feed::Dhan), 4);
        let (tx, rx) = tokio::sync::mpsc::channel::<CapturedFrame>(4);
        tx.send(CapturedFrame {
            seq: 1,
            endpoint: DhanEndpointType::Depth20,
            connection_index: 5,
            received_at: std::time::Instant::now(),
            bytes: bytes::Bytes::from_static(&[0x0C, 0x00, 0x29, 0x00, 0x0D, 0x00, 0x00, 0x00]),
        })
        .await
        .expect("send");
        drop(tx);

        let drained = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            run_frame_drain(
                rx,
                ingest,
                None,
                Arc::new(RingByteBudget::new(MAIN_FEED_RING_MAX_BYTES)),
                Arc::new(RingByteBudget::new(DEPTH_RING_MAX_BYTES)),
                Arc::new(tokio::sync::Notify::new()),
                Arc::new(tickvault_common::feed_health::FeedHealthRegistry::new()),
                tokio::sync::mpsc::channel(1).1,
            ),
        )
        .await
        .expect("a depth frame must never hang or panic the drain");

        // This test had ZERO assertions until 2026-08-11. Feeding a depth
        // frame to the main-feed dispatcher returns an Err rather than
        // panicking, so the misrouting the test is named for would have left
        // it green. It now pins the routing itself.
        assert_eq!(
            drained.depth_unconsumed, 1,
            "a depth frame must be routed by its endpoint and counted as unconsumed"
        );
        assert_eq!(
            drained.unparseable, 0,
            "a depth frame must NEVER reach the main-feed parser — its 12-byte header \
             parsed as an 8-byte one is the bug this routing exists to prevent, and it \
             shows up here as an unparseable count"
        );
        assert_eq!(
            drained.folded, 0,
            "depth is captured but not folded into ticks today"
        );
    }

    // -- structural proofs --------------------------------------------------

    #[test]
    fn an_offloaded_flush_never_touches_the_network_from_the_drain() {
        // `for_test` has NO ILP sender. Un-offloaded, a flush with pending
        // rows therefore takes the "QuestDB unreachable" arm, rescues the
        // buffer to disk and reports the rows as gone. Offloaded, the same
        // flush must be a queue hand-off that completes immediately — which
        // is the whole behavioural difference this change buys.
        let mut ingest = LiveIngest::new(TickWriter::for_test(Feed::Dhan), 4);
        let health = Arc::new(tickvault_common::feed_health::FeedHealthRegistry::new());
        ingest
            .spawn_offload_writer(Arc::clone(&health))
            .expect("the writer thread must spawn");

        assert!(
            ingest.writer_is_offloaded(),
            "the split must be observable — `flush_and_record` reads this flag \
             to decide whether the returned row count means LANDED or HANDED OFF"
        );

        let packet = ticker_packet(13, 23_146.45, 1_779_355_000);
        let parsed = dispatch_frame(&packet, 1_779_355_000_000_000_000)
            .expect("a well-formed ticker packet must parse");
        let ParsedFrame::Tick(tick) = parsed else {
            panic!("response code 2 must dispatch to a Tick");
        };
        ingest.ingest_tick(&tick, 42, 1_779_355_000_000);
        assert_eq!(ingest.pending_rows(), 1, "the tick must be buffered");

        let covered = ingest.flush();

        assert_eq!(covered, 1, "the flush covered the buffered row");
        assert_eq!(
            ingest.pending_rows(),
            0,
            "the rows left the drain's buffer on hand-off"
        );

        // Shutdown order under test: close the queue, then wait. This is the
        // step that makes the tail of a session durable rather than in-flight.
        ingest.shutdown_offload_writer();
        assert!(
            !ingest.writer_is_offloaded(),
            "after shutdown the lane falls back to the synchronous arm, so a \
             late flush rescues to disk instead of handing to a closed queue"
        );
    }

    #[test]
    fn the_writer_thread_is_joined_at_shutdown_so_the_tail_batch_lands() {
        // A thread that outlives its producer is a leak, and a thread that
        // exits EARLY is worse: the producer would keep handing rows to a
        // closed queue, which rescues every batch to disk while the lane looks
        // healthy. The only correct exit is "every sender is gone", so pin it.
        let mut ingest = LiveIngest::new(TickWriter::for_test(Feed::Dhan), 4);
        let health = Arc::new(tickvault_common::feed_health::FeedHealthRegistry::new());
        ingest
            .spawn_offload_writer(health)
            .expect("the writer thread must spawn");

        // `shutdown_offload_writer` joins. It would hang forever if closing
        // the queue did not actually wake the writer's blocking `recv`, so
        // reaching the next line at all is the assertion.
        ingest.shutdown_offload_writer();

        // Idempotent: a second call must not panic on an already-taken handle.
        ingest.shutdown_offload_writer();
    }

    #[test]
    fn test_drain_never_flushes_bare_on_the_async_worker() {
        // The drain task owns the ONLY consumer of the frame ring. Its
        // `flush()` is a SYNCHRONOUS blocking ILP-over-HTTP call bounded by
        // request_timeout=5000, so calling it bare pins a tokio worker — on a
        // 2-worker host that is half the runtime, shared with the WS read
        // loops that must keep pumping pongs. The drain then stops draining
        // and the 65,536-frame ring fills in ~13s at 5,000 fps; those frames
        // reach the WAL, which has no re-fold path, so they never become rows.
        //
        // Five sibling sites in this workspace already route through the
        // flavor-guarded helper. This one did not, and nothing caught it,
        // because "is this call blocking?" is invisible to the type system.
        let src = include_str!("dhan_feed_stack.rs");
        let test_marker = concat!("#[cfg(", "test)]");
        let production_half_with_comments = src.split(test_marker).next().unwrap_or(src);
        // Comments STRIPPED before counting (2026-08-18). The counting
        // assertions below search for call syntax, and this module documents
        // the old call shape while explaining why it changed — so an
        // un-stripped scan counts the explanation as a call and fails on the
        // very commit that fixes the defect. That is not hypothetical: it
        // happened here, when `flush_and_record`'s doc comment named the shape
        // it replaced.
        //
        // The `contains` assertions immediately below deliberately keep using
        // the UNSTRIPPED text: they look for declarations, and stripping would
        // not change their answer.
        let production_half = production_half_with_comments
            .lines()
            .map(|line| match line.find("//") {
                Some(idx) => &line[..idx],
                None => line,
            })
            .collect::<Vec<_>>()
            .join("\n");
        let production_half = production_half.as_str();

        assert!(
            production_half.contains("fn blocking_flush<T>"),
            "the flavor-guarded blocking_flush helper must exist"
        );
        assert!(
            production_half.contains("block_in_place"),
            "blocking_flush must actually move the flush off-worker on the \
             multi-thread runtime"
        );
        assert!(
            production_half.contains("RuntimeFlavor::MultiThread"),
            "the runtime-flavor guard is mandatory, not stylistic: \
             block_in_place PANICS on a current_thread runtime and this \
             module's drain tests are bare #[tokio::test]"
        );

        // The load-bearing assertion: no production call site may invoke a
        // BLOCKING flush() bare. Written as a search for the bare form so that
        // adding a further flush site without the helper fails here rather
        // than in prod.
        //
        // 2026-08-25 — an exception was carved for the offloaded early return,
        // on the reasoning that when the writer has been SPLIT
        // (`LiveIngest::spawn_offload_writer`) `ingest.flush()` performs no
        // network I/O: it hands the ILP buffer to a bounded queue that a
        // dedicated OS thread drains, so wrapping THAT in `block_in_place`
        // would spin up a replacement worker for a `try_send`.
        //
        // 2026-08-26 — THE EXCEPTION IS WITHDRAWN, because the reasoning was
        // wrong about what `LiveIngest::flush` does. It flushes the
        // inline-depth sink FIRST and unconditionally, above its own
        // `pending_rows == 0` early return, and `DepthIngest::flush` IS a
        // blocking ILP-over-HTTP round trip (`request_timeout=5000`). So the
        // carve-out did not exempt a queue hand-off — it exempted a 5-second
        // blocking HTTP call, on the ONLY path production takes, inside the
        // very test written to stop exactly that.
        //
        // This is worth more than the fix: the test DID hold the line it was
        // built to hold, and was then walked around by an exception written in
        // good faith from an incomplete reading of the callee. A guard is only
        // as strong as the claim its exception rests on, and this one rested
        // on a claim nobody re-derived from `LiveIngest::flush`.
        //
        // The invariant is therefore back to its original, unexceptional form:
        // EVERY production `ingest.flush()` is wrapped. `offloaded` is still
        // counted — pinned at ZERO — rather than deleted, so re-introducing
        // the bare early return fails here by name instead of quietly
        // rebalancing the equality.
        let bare = production_half.matches("ingest.flush()").count();
        let wrapped = production_half
            .matches("blocking_flush(|| ingest.flush())")
            .count();
        let offloaded = production_half.matches("return ingest.flush();").count();
        assert_eq!(
            offloaded, 0,
            "found {offloaded} BARE offloaded flush(es). `LiveIngest::flush` \
             flushes the inline-depth sink unconditionally and that is a \
             blocking ILP-over-HTTP call — a bare `return ingest.flush();` puts \
             it straight back on the async drain task, which is a tick-loss and \
             disconnect path, not merely a slow one"
        );
        assert!(
            production_half.contains("return blocking_flush(|| ingest.flush());"),
            "the offloaded early return must still EXIST and must be wrapped — \
             if it is gone entirely the writer split was deleted and the full \
             flush is back on the drain"
        );
        assert_eq!(
            bare,
            wrapped + offloaded,
            "every production ingest.flush() must be wrapped in blocking_flush; \
             found {bare} call(s), {wrapped} wrapped and {offloaded} bare — the \
             difference is a blocking HTTP call sitting on the async drain task"
        );

        // The offload must be WIRED, not merely available. Seven of the nine
        // findings fixed on 2026-08-25 were the same shape — a mechanism that
        // existed and was never plugged in — so a declaration on its own
        // proves nothing here either.
        assert!(
            production_half.contains("fn spawn_offload_writer"),
            "the writer-thread split must exist"
        );
        assert!(
            production_half.contains("ingest.spawn_offload_writer("),
            "the boot site must actually spawn the writer thread — a split that \
             is never performed leaves the blocking flush on the drain while \
             every symbol needed to move it sits right there unused"
        );
        // C4 of the recorded design, and the condition the LIVE MEASUREMENT
        // made load-bearing. A decoupled writer that accumulates without bound
        // widens each commit, and commit width is the measured amplifier — 10%
        // of a day's ticks carry an exchange timestamp over an hour behind
        // arrival, so a wide commit reopens closed hourly partitions and
        // rewrites them. Without the cap this change makes the disk pressure
        // it exists to relieve WORSE, which is why it is pinned here and not
        // left to a comment.
        let storage_src = include_str!("../../storage/src/tick_persistence.rs");
        assert!(
            storage_src.contains("MAX_RETAINED_FLUSH_SPANS"),
            "the batch-WIDTH cap must exist — unbounded accumulation under \
             backpressure is the own-goal this design was flagged for"
        );
        assert!(
            storage_src.contains("self.retained_spans > MAX_RETAINED_FLUSH_SPANS"),
            "the width cap must be ENFORCED on the retain path, not merely \
             declared — a constant nothing reads is the defect this repo has \
             now recorded seven times"
        );
        assert!(
            production_half.contains("OFFLOAD_SHUTDOWN_GRACE"),
            "the shutdown wait must be BOUNDED: `JoinHandle::join` has no \
             timeout, so a writer wedged on a hung socket would hang the box's \
             shutdown — worse, on a host whose auto-stop is a cost control, \
             than losing the tail batch"
        );
        assert!(
            production_half.contains("ingest.shutdown_offload_writer()"),
            "the drain must JOIN the writer thread after its tail flush — that \
             flush only HANDS OFF the last batch of the session, so without the \
             join the process can exit with it still in flight, losing exactly \
             the rows the tail flush exists to save"
        );
        assert!(
            production_half.contains("if ingest.writer_is_offloaded()"),
            "`flush_and_record` must gate on the offload flag: an offloaded \
             flush returns rows HANDED OFF, and recording those as feed health \
             forges liveness for exactly as long as the database is unreachable"
        );
        // 2026-08-18: the drain's three flush sites now route through
        // `flush_and_record`, which pairs the flush with the feed-health
        // report, so exactly ONE wrapped `ingest.flush()` remains — inside
        // that helper. This assertion used to require `wrapped >= 3` and
        // counted the call sites directly.
        //
        // The invariant is UNCHANGED and is now checked at a better place: a
        // single choke point is the only thing that can get the blocking call
        // wrong, so it is pinned exactly, and the site count is checked
        // separately below. Loosening `wrapped` alone would have been the
        // wrong edit — it would stop noticing a bare flush entirely — which is
        // why the equality above is kept and this pair replaces the count.
        //
        // 2026-08-26: TWO, not one. `flush_and_record` has two arms — the
        // offloaded early return and the synchronous fallback — and BOTH now
        // wrap, because the callee flushes the inline-depth sink over the
        // network on either path. Both wrapped calls are still inside the one
        // helper, so the choke-point property this assertion actually protects
        // is unchanged; only the arm count moved. Kept as an exact equality
        // rather than `>= 1` for the same reason it was exact before: a `>=`
        // would stop noticing the helper being inlined back into the drain.
        assert_eq!(
            wrapped, 2,
            "expected exactly TWO wrapped ingest.flush() calls — the offloaded \
             early return and the synchronous fallback, both inside \
             `flush_and_record`. Found {wrapped}: either a flush site bypassed \
             the helper, or the helper was inlined back into the drain (which \
             re-opens the four-sites-to-keep-in-sync problem)"
        );
        let recorded_sites = production_half
            .matches("flush_and_record(&mut ingest, &feed_health)")
            .count();
        assert!(
            recorded_sites >= 3,
            "expected at least the 3 known flush sites (size trigger, time \
             trigger, shutdown tail) routing through `flush_and_record`; found \
             {recorded_sites} — did a site get deleted, or start calling the \
             writer directly?"
        );
    }

    #[test]
    fn test_stack_never_reaches_for_an_instrument_download() {
        // Q3 of the 2026-07-13 amendment: hardcoded security ids only. This is
        // the mechanical half of that promise.
        let src = include_str!("dhan_feed_stack.rs");
        let test_marker = concat!("#[cfg(", "test)]");
        let production_half = src.split(test_marker).next().unwrap_or(src);
        assert!(production_half.contains("SPOT_1M_REST_INDICES"), "sanity");
        for banned in [
            concat!("csv_", "downloader"),
            concat!("csv_", "parser"),
            concat!("api-scrip-", "master"),
            concat!("Subscription", "Scope"),
            concat!("LOCKED_", "UNIVERSE"),
            // The instrument-master CSV host. THIS is the thing Q3 forbids —
            // an instrument download — and it stays banned by name.
            concat!("images.", "dhan.co"),
        ] {
            assert!(
                !production_half.contains(banned),
                "the live-feed stack must never reach for an instrument download; found \
                 `{banned}`"
            );
        }

        // NARROWED 2026-08-11, deliberately and with the reason recorded.
        //
        // This list used to ban `reqwest::` outright, as a blunt proxy for "no
        // downloads". That proxy was wrong in a way that mattered: the 15:31
        // cross-verification MUST make an HTTP call — it compares our captured
        // candles against Dhan's own REST record, and it is the only ground
        // truth this lane has (the main feed carries no sequence number and no
        // snapshot-on-subscribe). Banning all HTTP would have banned the
        // verifier that `websocket-connection-scope-lock.md` requires to be
        // live from day one.
        //
        // So the ban moves from the TRANSPORT to the TARGET, which is what Q3
        // actually cares about: no instrument-master host, no CSV downloader,
        // no parser, no universe enum. HTTP to the authorized intraday-candles
        // endpoint (a KEEP class under `no-rest-except-live-feed-2026-06-27.md`
        // §8) is permitted — and pinned below to that one use, so this cannot
        // quietly become a general licence to fetch.
        let http_uses = production_half.matches(concat!("reqwest", "::")).count();
        assert!(
            http_uses <= 2,
            "HTTP in this module is permitted ONLY for the 15:31 comparator (a Client type \
             and its constructor). Found {http_uses} uses — if a new one is legitimate, say \
             why here; if it is a fetch of instrument data, it violates Q3."
        );
        for (needle, why) in [
            (
                concat!("run_cross_", "verification"),
                "the comparator must still be the caller",
            ),
            (
                "intraday_url",
                "the only endpoint this module may reach is the authorized intraday one",
            ),
        ] {
            assert!(
                production_half.contains(needle),
                "the HTTP allowance is scoped to the comparator, but {why} — missing \
                 `{needle}`"
            );
        }
    }

    /// The market-hours gate on the silence PAGE, at its exact boundaries.
    ///
    /// Pure and total, so the edges are pinned without a clock. The gate is
    /// half-open on purpose: 09:00:00 is in, 15:40:00 is out. An inclusive
    /// upper bound would page on the very second the persistence window
    /// closes, which is the one second the whole universe is guaranteed to
    /// look silent.
    #[test]
    fn test_is_within_market_hours_ist_gates_the_silence_page_to_continuous_trading() {
        let open = CONTINUOUS_SESSION_START_SECS_OF_DAY_IST;
        let close = u64::from(TICK_PERSIST_END_SECS_OF_DAY_IST);

        assert!(!is_within_market_hours_ist(0), "midnight is not in session");
        assert!(
            !is_within_market_hours_ist(open - 1),
            "one second before the window opens must NOT page"
        );
        assert!(
            is_within_market_hours_ist(open),
            "the opening second is in session"
        );
        // The pre-open window is the reason this gate is not simply the
        // persistence window: nothing trades between 09:00 and 09:15, so
        // every instrument is legitimately silent and a page there would
        // fire every single trading morning.
        assert!(
            !is_within_market_hours_ist(u64::from(TICK_PERSIST_START_SECS_OF_DAY_IST)),
            "09:00 opens PERSISTENCE, not trading — silence is expected here"
        );
        assert!(
            u64::from(TICK_PERSIST_START_SECS_OF_DAY_IST) < open,
            "precondition: the persistence window really does open earlier"
        );
        assert!(
            is_within_market_hours_ist(close - 1),
            "the last second before close is still in session"
        );
        assert!(
            !is_within_market_hours_ist(close),
            "the closing second is OUT — every instrument goes quiet here by \
             design, and paging for it would train the operator to ignore this \
             alert entirely"
        );
        assert!(
            !is_within_market_hours_ist(86_399),
            "the end of the day is not in session"
        );
    }

    /// The scan cadence must not be able to outrun what the detector can
    /// actually judge.
    #[test]
    fn test_silence_scan_interval_is_not_faster_than_the_detector_floor() {
        let floor_millis =
            u128::from(tickvault_core::pipeline::tick_gap_detector::DEFAULT_SILENCE_FLOOR_MILLIS);
        assert!(
            SILENCE_SCAN_INTERVAL.as_millis() >= floor_millis,
            "scanning every {:?} cannot surface anything the detector could not \
             already judge at a {floor_millis} ms floor — it would only burn an \
             O(n) sweep for no new signal",
            SILENCE_SCAN_INTERVAL
        );
        assert!(
            SILENCE_SCANS_BEFORE_ALERT >= 2,
            "one scan is not evidence: a scan landing in the shadow of a \
             reconnect sees instruments that are legitimately not ticking yet"
        );
    }

    /// The read-out exists and is wired into the drain loop.
    ///
    /// This is the ratchet for the defect the whole feature answers: the
    /// detector was seeded and fed on every tick while `scan_silence` had
    /// ZERO production callers. A wired sensor with no read-out reads greener
    /// than dead code, so the wiring itself has to be pinned.
    #[test]
    fn test_the_drain_actually_asks_the_detector_what_it_recorded() {
        let src = include_str!("dhan_feed_stack.rs");
        let body = src
            .split_once("async fn run_frame_drain")
            .expect("the drain function must exist")
            .1;
        for needle in [
            "silence_timer.tick()",
            "ingest.scan_silence(",
            "is_within_market_hours_ist(",
            "SILENCE_SCANS_BEFORE_ALERT",
        ] {
            assert!(
                body.contains(needle),
                "run_frame_drain must contain `{needle}` — without it the gap \
                 detector is fed on every tick and never questioned, which is \
                 exactly the defect this scan was added to close"
            );
        }
        // And the scan must NOT ride the 500 ms flush arm: it is O(n) in
        // tracked instruments, so at 25,000 instruments that would be a
        // 25,000-element sweep twice a second.
        assert!(
            body.contains("let mut silence_timer"),
            "the silence scan must have its OWN timer, never the flush timer"
        );
    }

    #[test]
    fn test_up_gauge_is_raised_by_a_received_frame_and_never_by_a_spawn() {
        // REWRITTEN 2026-08-20. The previous version of this guard asserted
        // the raise came AFTER the dial loop — and that is exactly what made
        // the defect permanent, because it pinned the wrong thing as correct.
        //
        // `dialed` counts `tokio::spawn` calls; the connect happens INSIDE the
        // spawned task. So a socket answering HTTP 400 forever still counted
        // as dialed, the gauge read 1.0, and `tv-<env>-dhan-live-lane-down`
        // stayed silent. On 2026-08-12 that is precisely what a 373-minute,
        // zero-candle session looked like from CloudWatch: healthy.
        //
        // The invariant is now about PROVENANCE, not source order: only code
        // that is holding a received frame may claim the lane is up.
        let src = include_str!("dhan_feed_stack.rs");
        let test_marker = concat!("#[cfg(", "test)]");
        let production_half = src.split(test_marker).next().unwrap_or(src);

        assert_eq!(
            production_half
                .matches("FEED_STACK_UP_GAUGE).set(1.0)")
                .count(),
            1,
            "exactly ONE site may raise the up-gauge, so there is one place to audit"
        );

        // THE LOAD-BEARING ASSERTION. The raise must live inside the drain,
        // which is the only code in the process that has proof a socket both
        // connected AND delivered — it is holding the bytes.
        let drain = production_half
            .split_once("async fn run_frame_drain")
            .expect("the drain function must exist")
            .1;
        assert!(
            drain.contains("gauge!(FEED_STACK_UP_GAUGE).set(1.0)"),
            "the up-gauge must be raised INSIDE run_frame_drain, on a frame that \
             actually arrived — anywhere else is reporting an intention"
        );
        assert!(
            drain.contains("if !lane_up"),
            "the raise must be latched, so the rising edge is one event"
        );

        // And the bring-up path must NOT raise it. This is the half that
        // regressed: a `.set(1.0)` here means 'we spawned some tasks', which
        // is not connectivity and must never be published as if it were.
        let bringup = production_half
            .rsplit_once("fn dial_planned_connections")
            .map_or(production_half, |(before, _)| before);
        let dial_marker = "dialed = dialed.saturating_add(1)";
        if let Some(dialed_at) = bringup.find(dial_marker) {
            let after_dial = &bringup[dialed_at..];
            assert!(
                !after_dial.contains("FEED_STACK_UP_GAUGE).set(1.0)"),
                "the dial loop must not raise the up-gauge — `dialed` counts \
                 tokio::spawn calls, and a socket that 400s forever still \
                 increments it"
            );
        }

        // Something must still clear it, or a lane whose every socket died
        // would sit at 1 until the process restarted.
        assert!(
            production_half
                .matches("FEED_STACK_UP_GAUGE).set(0.0)")
                .count()
                >= 2,
            "both the drain's clean exit AND the dead-drain handler must clear the gauge — \
             a drain that panics would otherwise leave the lane reporting itself up"
        );
    }

    #[test]
    fn test_main_feed_connections_for_mirrors_what_plan_pool_actually_dials() {
        // THE REGRESSION. `plan_pool` SPREADS: it takes
        // `min(available, set.len()).max(1)` connections. This function used
        // to return the PACKED count, `ceil(len / 5000)`. At the live scale
        // those disagree 5 vs 1, so the attach was told four connections were
        // free when zero were — and the resulting BudgetRefused aborted the
        // whole plan before depth was ever planned.
        let max = usize::from(tickvault_core::websocket::pool_budget::MAX_MAIN_FEED_CONNECTIONS);

        assert_eq!(
            main_feed_connections_for(0, max),
            0,
            "an empty set occupies nothing"
        );
        assert_eq!(
            main_feed_connections_for(1, max),
            1,
            "one instrument still occupies a whole connection"
        );
        // The live universe. The main feed PACKS, so 4,565 spots occupy ONE
        // connection and leave four for the contract pass.
        assert_eq!(
            main_feed_connections_for(4_565, max),
            1,
            "4,565 fits one 5,000-instrument socket — packing pass 1 is what \
             leaves room for the ~20,000 contracts of pass 2"
        );
        assert_eq!(
            main_feed_connections_for(25_000, max),
            max,
            "at the target scale packed and spread converge exactly"
        );
        // Never more than are actually free, whatever the arithmetic says.
        assert_eq!(
            main_feed_connections_for(25_000, 2),
            2,
            "clamped to what the pool has left — reporting 5 here is what \
             asked for room that did not exist and refused the whole pool"
        );
        assert_eq!(main_feed_connections_for(3, max), 1, "three fit one socket");

        // THE POINT OF THE WHOLE FIX: after the spots, contracts still fit.
        let used = main_feed_connections_for(4_565, max);
        let room = remaining_main_feed_capacity(used);
        assert_eq!(
            room, 20_000,
            "four connections x 5,000 remain for contracts"
        );
        assert_eq!(
            used + main_feed_connections_for(room, max.saturating_sub(used)),
            max,
            "spots + contracts together fill all five main-feed sockets — the \
             spread directive's intent, delivered by packing the first pass"
        );
    }
    #[test]
    fn test_the_lane_refuses_to_dial_without_a_wal_or_a_token_manager() {
        // Both refusals are `return`s on the bring-up path, not warnings that
        // fall through. Capturing without a durable floor, or dialing with a
        // blank credential, would each look like success while being neither —
        // so this pins that the code says REFUSING and means it.
        let src = include_str!("dhan_feed_stack.rs");
        let test_marker = concat!("#[cfg(", "test)]");
        let production_half = src.split(test_marker).next().unwrap_or(src);

        for needle in [
            "let Some(spill) = params.spill else",
            "let Some(client_id) = client_id else",
        ] {
            let at = production_half
                .find(needle)
                .unwrap_or_else(|| panic!("the bring-up must still guard on `{needle}`"));
            let tail = &production_half[at..];
            let block_end = tail.find("};").unwrap_or(tail.len());
            assert!(
                tail[..block_end].contains("return;"),
                "`{needle}` must REFUSE the lane, never warn and continue"
            );
        }

        // The token-manager guard must WAIT before it refuses.
        //
        // This arm used to pin `let Some(client_id) = global_token_manager()`
        // — the bare one-shot check — and it passed happily while the lane
        // lost a race it could not win: the registrar sits behind an SSM +
        // TOTP + HTTPS auth round-trip, and this lane is spawned with a
        // single localhost query between them. A guard that is CORRECT
        // (refusing without a credential is right) can still be WRONG in
        // timing, and a source pin on the refusal alone cannot tell the
        // difference. Pin the wait too.
        assert!(
            production_half.contains("for attempt in 0..TOKEN_MANAGER_WAIT_ATTEMPTS"),
            "the lane must RETRY for the token manager, not one-shot it — the \
             registrar completes a remote auth handshake first, so a single \
             check loses on essentially every boot"
        );
        assert!(
            production_half.contains("tokio::time::sleep"),
            "the token-manager wait must actually yield between attempts"
        );
        assert!(
            TOKEN_MANAGER_WAIT_ATTEMPTS * TOKEN_MANAGER_WAIT_INTERVAL_SECS >= 260,
            "the wait budget must cover at least two TokenManager::initialize \
             attempts across its >=130s backoff floor, or one transient auth \
             failure still costs the whole session"
        );
    }

    /// The 15:31 comparator's day origin must be IST-WALL-AS-EPOCH, because
    /// that is how both compared sides stamp their minutes.
    ///
    /// This test exists because the origin was built with
    /// `.and_local_timezone(ist)` — the TRUE UTC instant of IST midnight —
    /// while `ticks.ts` is `exchange_timestamp * 1e9` with no offset and the
    /// REST side adds the offset to a UTC epoch. The 19,800-second skew pushed
    /// every bucket from 10:00 IST onward out of `is_in_session`, so the only
    /// loss detector this lane has compared 45 of 375 minutes and still
    /// reported Clean.
    ///
    /// The assertion below is deliberately end-to-end over the WHOLE session
    /// rather than a spot check on the origin: a test that only asserted
    /// "origin == some constant" would have been satisfied by the broken value
    /// too, as long as the constant were derived the same broken way.
    #[test]
    fn test_crossverify_day_origin_covers_the_entire_session_not_just_the_first_45_minutes() {
        use crate::dhan_live_crossverify::{
            SESSION_CLOSE_SECS_OF_DAY_IST, SESSION_OPEN_SECS_OF_DAY_IST, is_in_session,
            is_tail_minute,
        };

        let day = chrono::NaiveDate::from_ymd_opt(2026, 8, 11).expect("date"); // APPROVED: test
        // Built EXACTLY as the runner builds it.
        let origin = day
            .and_hms_opt(0, 0, 0)
            .and_then(|dt| dt.and_utc().timestamp_nanos_opt())
            .expect("origin"); // APPROVED: test

        // The DATA's stamping convention — fixed, and DELIBERATELY not derived
        // from `origin`.
        //
        // The first draft of this test built `ts` from `origin`, so `ts -
        // origin` cancelled the origin entirely and the assertion held for ANY
        // origin. It passed with the bug deliberately re-injected. A test whose
        // subject cancels out of its own arithmetic proves nothing — which is
        // exactly the class this audit was hunting, found in the test written
        // to close it.
        //
        // `ticks.ts` is `exchange_timestamp * 1e9`, and Dhan's LTT is already
        // IST epoch seconds, so an IST wall-clock time is stamped as though it
        // were UTC. `and_utc()` on the IST date reproduces that.
        let data_midnight_secs = day
            .and_hms_opt(0, 0, 0)
            .and_then(|dt| dt.and_utc().timestamp_nanos_opt())
            .expect("data midnight") // APPROVED: test
            / 1_000_000_000;
        let stamp = |h: i64, mi: i64| (data_midnight_secs + h * 3600 + mi * 60) * 1_000_000_000;

        let mut in_session = 0i64;
        for m in 0..(24 * 60) {
            let ts = (data_midnight_secs + i64::from(m) * 60) * 1_000_000_000;
            if is_in_session(ts, origin) {
                in_session += 1;
            }
        }
        // 09:15..15:40 = 385 minutes. DERIVED, never a hand-typed literal:
        // this count moved once already (375 -> 385) when NSE added the
        // 15:30-15:40 closing session on 2026-08-07, and a literal is exactly
        // what let the private duplicate close-constant miss that migration
        // for eighteen days.
        let expected_minutes = (SESSION_CLOSE_SECS_OF_DAY_IST - SESSION_OPEN_SECS_OF_DAY_IST) / 60;
        assert_eq!(
            in_session, expected_minutes,
            "the session gate must accept all {expected_minutes} session minutes. \
             Got {in_session} — a count near 45 is the +19,800s IST-origin skew returning."
        );

        // And the tail amnesty must land on the REAL tail — the last two
        // session minutes, whatever the close currently is — never on
        // 09:58/09:59 as it did under the skew.
        let tail_at = |h: i64, mi: i64| is_tail_minute(stamp(h, mi), origin);
        let hm = |secs: i64| (secs / 3600, (secs % 3600) / 60);
        let (h1, m1) = hm(SESSION_CLOSE_SECS_OF_DAY_IST - 60);
        let (h2, m2) = hm(SESSION_CLOSE_SECS_OF_DAY_IST - 120);
        assert!(tail_at(h1, m1), "{h1}:{m1} must be tail-amnestied");
        assert!(tail_at(h2, m2), "{h2}:{m2} must be tail-amnestied");
        assert!(
            !tail_at(9, 58),
            "09:58 is NOT the tail — that is the skew signature"
        );
        assert!(
            !tail_at(9, 59),
            "09:59 is NOT the tail — that is the skew signature"
        );
    }

    /// THE regression that would cost real ticks: the depth late-attach must
    /// never sit between the main-feed dial and the ring's template drop.
    ///
    /// Depth waits until ~09:16 IST. If that wait were inline, the main feed
    /// would dial 45 minutes late and the lane would miss the open — trading 5
    /// working sockets for 0, which is strictly worse than the 5-of-16 this

    #[test]
    fn the_cross_verify_verdict_is_logged_as_fields_not_a_debug_dump() {
        // MEASURED DEFECT, prod box 2026-08-20: this site emitted `?report`,
        // and the resulting log line was 1,048,374 characters — EXACTLY
        // CloudWatch's 1 MiB event ceiling, therefore truncated.
        //
        // `RunReport`'s derived Debug prints `comparison.findings` BEFORE the
        // totals, so the truncation ate precisely the summary. `minutes_compared`
        // — the non-vacuity denominator, the one number that decides whether
        // the day's captured candles were verified at all — was unreadable,
        // while several thousand individual findings were not.
        //
        // A source scan rather than a runtime assertion because the emit sits
        // inside the live cross-verify arm, which needs a token, QuestDB and a
        // real REST leg to reach.
        let src = include_str!("dhan_feed_stack.rs");
        let marker = "cross-verification finished — this is the honest";
        let idx = src.find(marker).expect("the cross-verify emit must exist");
        // CORRECTED 2026-08-26. This walked back a FIXED 2,000 bytes, and on
        // the day four more fields were added to the emit it failed — not
        // because a field was missing, but because the list outgrew the
        // window. It failed CLOSED, blocking a correct change, which is the
        // better of the two directions; the shape is wrong either way.
        //
        // This is the FIFTH fixed-window guard found in this branch, all
        // written by me. A byte count is a guess about how long code will
        // stay; the macro opening is a real boundary and cannot drift.
        //
        // The openings are assembled from FRAGMENTS, not written whole. A
        // source-scanning guard in another crate reads this file for `error!`
        // sites and cannot tell a literal in a test array from a real emit —
        // it failed on exactly that, and the failure was 100% correct given
        // what it could see. Same technique the failure-reason pin uses one
        // module over, for the same reason: this file is read by scanners.
        let start = [
            concat!("info", "!("),
            concat!("error", "!("),
            concat!("warn", "!("),
        ]
        .iter()
        .filter_map(|m| src[..idx].rfind(m))
        .max()
        .expect("the emit must sit inside a tracing macro");
        let emit = &src[start..idx];

        assert!(
            emit.contains("minutes_compared = c.minutes_compared"),
            "the non-vacuity denominator must be its own field — it is the number \
             the 1 MiB truncation destroyed"
        );
        for field in [
            "cells_diverged = c.cells_diverged",
            "missing_live = c.missing_live",
            "missing_rest = c.missing_rest",
            "noise_p50_paise = c.noise_p50_paise",
            "vacuous = c.is_vacuous()",
        ] {
            assert!(
                emit.contains(field),
                "the verdict emit lost `{field}` — every summary number must be a \
                 bounded named field, not part of a dump that can be cut off"
            );
        }
        assert!(
            !emit.contains("?report"),
            "the verdict must NOT debug-dump the whole report: its findings vector \
             is unbounded and pushed the line onto CloudWatch's 1 MiB ceiling, \
             truncating away the totals that follow it"
        );
    }
    /// change exists to improve on.
    #[test]
    fn test_depth_late_attach_cannot_delay_the_main_feed_dial() {
        let src = include_str!("dhan_feed_stack.rs");
        let test_marker = concat!("#[cfg(", "test)]");
        let production = src.split(test_marker).next().unwrap_or(src);
        let main_dial = production
            .find("MAIN-FEED-DIAL-SITE")
            .expect("the main-feed dial call site must exist");
        let depth_attach = production
            .find("tokio::spawn(attach_depth_when_available(")
            .expect("the depth late-attach SPAWN SITE must exist (anchor on the call, never the definition — the helper is defined above the bring-up)");
        assert!(
            main_dial < depth_attach,
            "the main feed must be dialed BEFORE the depth late-attach is set up — depth waits \
             ~45 minutes for the option-chain leg, and doing that first would cost the market open"
        );
        assert!(
            production.contains("tokio::spawn(attach_depth_when_available("),
            "the depth late-attach MUST be spawned, never awaited inline — an inline await \
             would block the bring-up (and therefore the template-sender drop) for the whole wait"
        );
    }

    /// THE BITE for the 2026-08-21 depth-starvation defect.
    ///
    /// This state — contracts already on the wire, depth still resolving — is
    /// the one the pre-fix code could not express. A single `anything_resolved`
    /// boolean gated both halves and a single `return` ended the loop, so the
    /// moment contracts dialed (~09:15:30, off the `ticks` table) the task
    /// exited and depth (which cannot resolve before the 09:16:00 chain fire)
    /// never dialed at all. Ten authorized sockets, dark every session.
    #[test]
    fn outstanding_halves_lets_depth_dial_after_contracts_already_did() {
        assert_eq!(
            outstanding_halves(true, true, false, true, false),
            (false, true),
            "with contracts dialed and depth resolved, DEPTH must still dial. Returning \
             (false, false) here is precisely the defect: it is what made the loop exit \
             before depth ever reached the wire."
        );
    }

    /// `pool.admit` is stateful, so re-planning a dialed half consumes a second
    /// set of connection slots and re-sends an overflow the live socket holds.
    #[test]
    fn outstanding_halves_never_redials_a_finished_half() {
        assert_eq!(
            outstanding_halves(true, true, true, true, false),
            (false, false),
            "both halves are done; neither may be planned again"
        );
    }

    /// The pricing quorum holds CONTRACTS. It must never hold DEPTH.
    ///
    /// **CORRECTED 2026-08-26.** This said depth "reads the option chain, not
    /// spot prices". Both halves of that are now false: depth reads the
    /// CONTRACT ARTIFACT (`load_depth_candidates` -> `read_contract_artifact`,
    /// with the chain only as an unreachable fallback), and it very much does
    /// read spot prices — `fetch_spot_prices` is how at-the-money is located
    /// at all. The test was right; its reason was not, which is worse than no
    /// reason, because a reader checking whether the coupling still matters
    /// would have been told to look at the wrong thing.
    ///
    /// The REAL reason is a timing one, and it is what makes the operator's
    /// 09:13 deadline reachable. Depth needs spot prices, which exist from
    /// 09:00 — pre-open ticks are persisted even though they are not folded
    /// into candles. The CONTRACT half additionally waits on a pricing quorum
    /// across ~208 stock underlyings so the at-the-money window is not sized
    /// against a near-empty price map. Those are different questions with
    /// different answers at 09:13, so coupling them would make depth wait for
    /// something it never needed.
    #[test]
    fn outstanding_halves_holds_contracts_while_pending_but_never_depth() {
        assert_eq!(
            outstanding_halves(true, true, false, false, true),
            (false, true),
            "a pending contract selection must not re-starve depth — that would trade one \
             defect for the other"
        );
    }

    /// Non-vacuity: the ordinary both-outstanding case must dial both.
    #[test]
    fn outstanding_halves_dials_both_when_both_are_ready() {
        assert_eq!(
            outstanding_halves(true, true, false, false, false),
            (true, true)
        );
        assert_eq!(
            outstanding_halves(false, false, false, false, false),
            (false, false),
            "nothing resolved means nothing to dial"
        );
    }

    /// The depth task must hold a WEAK sender.
    ///
    /// A held `Sender` clone keeps the frame ring OPEN for the whole wait, so
    /// if every main-feed socket died in that window the drain could not close
    /// and the lane would read alive while producing nothing — the same
    /// false-OK this lane's observability work exists to remove, arriving from
    /// a new direction.
    #[test]
    fn test_depth_late_attach_holds_only_a_weak_sender() {
        let src = include_str!("dhan_feed_stack.rs");
        let test_marker = concat!("#[cfg(", "test)]");
        let production = src.split(test_marker).next().unwrap_or(src);
        assert!(
            production.contains("frame_tx.downgrade()"),
            "the depth late-attach must be handed a WeakSender via downgrade(), never a clone"
        );
        assert!(
            production.contains("frame_weak.upgrade()"),
            "the depth task must upgrade() at dial time so it declines to dial into a ring that \
             closed while it waited"
        );
        // The template drop must still be present and must NOT have been moved
        // behind the attach: that drop is what lets the ring close at all.
        let downgrade = production
            .find("frame_tx.downgrade()")
            .expect("downgrade site");
        let drop_site = production
            .find("drop(frame_tx);")
            .expect("the template-sender drop must still exist");
        assert!(
            downgrade < drop_site,
            "downgrade() must happen before the template sender is dropped"
        );
    }

    /// The deadline must gate RETRIES, never the FIRST attempt.
    ///
    /// A mid-session restart (a 12:30 IST redeploy, say) is already past the
    /// 10:00 IST deadline — but that is exactly when `option_chain_1m` is
    /// FULLEST, having filled since 09:16. Checking the deadline before
    /// attempt 1 made depth give up without asking, leaving it dark after
    /// every intra-day restart: the precise failure the late-attach exists to
    /// end. Caught before deploying only because the deploy was mid-session.
    #[test]
    fn test_depth_deadline_gates_retries_not_the_first_attempt() {
        let src = include_str!("dhan_feed_stack.rs");
        let test_marker = concat!("#[cfg(", "test)]");
        let production = src.split(test_marker).next().unwrap_or(src);
        assert!(
            production.contains("let out_of_time = past_hard_stop || past_deadline_and_window;"),
            "the two deadline conditions must still be ORed into one `out_of_time` value — the \
             give-up arm and the pending-hold arm both read it, and computing them separately \
             would let the two disagree about whether waiting is still allowed"
        );
        assert!(
            production.contains("if attempts > 0 && out_of_time && !last_had_instruments {"),
            "the depth give-up MUST be guarded by `attempts > 0` — an unguarded check makes a \
             mid-session restart give up before it has looked even once, exactly when the chain \
             table is fullest. It must ALSO be guarded by `!last_had_instruments`: since \
             2026-08-20 the loop deliberately holds out for a complete contract selection, and \
             a give-up that discarded an already-resolved partial set would turn waiting for \
             better into losing what we had"
        );
    }

    /// The wall-clock deadline alone must not be able to end the task.
    ///
    /// Prod, 2026-08-15: the app started at 10:01 IST, took its one permitted
    /// attempt 20 seconds later — before the option-chain leg had fired even
    /// once — and the 10:00 deadline then cancelled every retry. One doomed
    /// look, dark for the session, `attempts: 1` in the log.
    ///
    /// A source scan rather than a behavioural test because the alternative is
    /// a 30-minute sleep or a clock injection through five call sites; the
    /// condition is a pure boolean and its shape is the whole fix.
    #[test]
    fn test_depth_give_up_requires_both_the_deadline_and_a_minimum_window() {
        let src = include_str!("dhan_feed_stack.rs");
        let test_marker = concat!("#[cfg(", "test)]");
        let production = src.split(test_marker).next().unwrap_or(src);

        assert!(
            production.contains("window_elapsed >= DEPTH_ATTACH_MIN_WINDOW_SECS"),
            "the give-up condition no longer requires a minimum window since THIS task started. \
             Without it, any start after ~09:59 IST gets exactly one attempt, taken seconds \
             after boot when the chain leg cannot yet have run — guaranteed empty, then \
             permanently dark"
        );
        assert!(
            production.contains("now_ist >= DEPTH_ATTACH_DEADLINE_IST_SECS")
                && production.contains("&& window_elapsed"),
            "the deadline and the window must be ANDed. ORing them restores the 2026-08-15 \
             defect: the clock alone would again be sufficient to give up"
        );
        assert!(
            production.contains("past_hard_stop"),
            "the minimum window must be bounded by a hard stop, or a 15:25 restart would keep \
             this task polling into the evening for contracts that will not trade again today"
        );
    }

    #[test]
    fn test_depth_attach_windows_are_ordered_and_inside_the_session() {
        // The three constants only make sense in one order, and getting it
        // wrong is silent: a hard stop below the deadline would end the task
        // before the deadline could ever be reached, making the deadline dead
        // code that reads as if it were live.
        assert!(
            DEPTH_ATTACH_DEADLINE_IST_SECS < DEPTH_ATTACH_HARD_STOP_IST_SECS,
            "the hard stop must be AFTER the deadline, or the deadline is unreachable"
        );
        assert!(
            DEPTH_ATTACH_HARD_STOP_IST_SECS <= 15 * 3_600 + 30 * 60,
            "the hard stop must not run past the 15:30 IST close — depth on a closed market \
             subscribes contracts that will not trade again today"
        );
        assert!(
            DEPTH_ATTACH_MIN_WINDOW_SECS >= 10 * DEPTH_ATTACH_RETRY_SECS,
            "the minimum window must allow at least ten polls, or a transient QuestDB stall \
             consumes the whole allowance and depth gives up on a healthy chain"
        );
        // And the window must fit inside the session from the deadline, or a
        // start at exactly the deadline would be cut short by the hard stop.
        let from_deadline_to_close =
            u64::from(DEPTH_ATTACH_HARD_STOP_IST_SECS - DEPTH_ATTACH_DEADLINE_IST_SECS);
        assert!(
            DEPTH_ATTACH_MIN_WINDOW_SECS <= from_deadline_to_close,
            "the minimum window ({DEPTH_ATTACH_MIN_WINDOW_SECS}s) is longer than the time \
             between the deadline and the close ({from_deadline_to_close}s), so a start at the \
             deadline could never use its full allowance"
        );
    }
}

#[cfg(test)]
mod wal_refold_tests {
    use super::*;
    use tickvault_common::feed::Feed;

    fn ingest() -> LiveIngest {
        LiveIngest::new(TickWriter::for_test(Feed::Dhan), 4)
    }

    #[test]
    fn test_refold_wal_frames_empty_batch_recovers_nothing() {
        let out = refold_wal_frames(&mut ingest(), &[]);
        assert_eq!(out, WalRefoldOutcome::default());
    }

    #[test]
    fn test_refold_wal_frames_unparseable_frame_is_counted() {
        // A frame whose first byte is not a known packet code. The batch must
        // survive it — one corrupt frame must never cost the recovery of the
        // frames beside it — but it must NOT vanish silently either.
        let frames = vec![(1u64, bytes::Bytes::from_static(&[0xFF, 0xFF, 0xFF, 0xFF]))];
        let out = refold_wal_frames(&mut ingest(), &frames);
        assert_eq!(out.refolded, 0, "garbage must not produce ticks");
        assert_eq!(
            out.unparseable, 1,
            "garbage must be COUNTED, not dropped silently"
        );
    }

    #[test]
    fn test_refold_wal_frames_truncated_packet_stops_at_boundary() {
        // Claims a ticker packet but carries fewer bytes than one. The walker
        // must stop at the boundary rather than reading past it or
        // resynchronising on a guess, which would fabricate ticks.
        let frames = vec![(7u64, bytes::Bytes::from_static(&[2, 0, 0, 0]))];
        let out = refold_wal_frames(&mut ingest(), &frames);
        assert_eq!(out.refolded, 0);
        assert!(out.unparseable >= 1, "a truncated packet must be counted");
    }

    #[test]
    fn test_wal_refold_outcome_refolded_and_lost_mutually_exclusive() {
        // The arithmetic guarantee the operator relies on: a tick is folded
        // XOR lost. If both could increment for one tick, a loss report could
        // be hidden behind a success count.
        let out = refold_wal_frames(&mut ingest(), &[]);
        assert_eq!(out.refolded, 0);
        assert_eq!(out.lost, 0);
        // Structural: the fold loop's match arms are disjoint by construction —
        // `Folded` increments `refolded`, every other outcome increments `lost`,
        // and there is no arm that touches both.
        let src = include_str!("dhan_feed_stack.rs");
        let body = src
            .split("pub fn refold_wal_frames")
            .nth(1)
            .expect("refold_wal_frames must exist");
        let loop_body = &body[..body.find("metrics::counter!").unwrap_or(body.len())];
        assert_eq!(
            loop_body.matches("out.refolded = out.refolded").count(),
            1,
            "exactly one site may increment `refolded`"
        );
        assert_eq!(
            loop_body.matches("out.lost = out.lost").count(),
            1,
            "exactly one site may increment `lost`"
        );
    }
}

#[cfg(test)]
mod host_sizing_tests {
    use super::*;

    #[test]
    fn test_frame_ring_max_bytes_for_host_never_below_the_proven_floor() {
        // The floor is today's fixed value. Auto-sizing may only GROW the
        // budget, so a small host — or one whose memory cannot be read — lands
        // exactly where it is now. This is what makes the change unable to
        // regress a working configuration.
        assert!(frame_ring_max_bytes_for_host() >= FRAME_RING_MAX_BYTES);
    }

    #[test]
    fn test_frame_ring_max_bytes_for_host_never_above_the_ceiling() {
        // Above the ceiling the ring stops absorbing bursts and starts
        // competing with the database for the same RAM.
        assert!(frame_ring_max_bytes_for_host() <= FRAME_RING_MAX_BYTES_CEILING);
    }

    #[test]
    fn test_host_total_ram_is_readable_and_plausible() {
        // Non-vacuity: if this returned None everywhere, the two bounds tests
        // above would pass while the sizing never actually did anything.
        // Plausibility bounds rather than an exact value, because CI runners
        // and the prod box differ.
        match host_total_ram_bytes() {
            Some(total) => {
                assert!(
                    total >= 256 * 1024 * 1024,
                    "implausibly small MemTotal ({total} bytes) — parse is wrong"
                );
                assert!(
                    total <= 8 * 1024 * 1024 * 1024 * 1024,
                    "implausibly large MemTotal ({total} bytes) — unit is wrong \
                     (kB vs bytes is the classic error here)"
                );
            }
            None => {
                // Acceptable on a non-Linux or restricted host; the fallback is
                // the floor, which the first test already pins.
            }
        }
    }

    const GIB: usize = 1024 * 1024 * 1024;

    #[test]
    fn test_ring_sizing_is_monotonic_in_host_ram() {
        // The property that matters: a bigger box gets at least as much ring.
        // Asserted against the REAL sizing function — an earlier version of
        // this test re-implemented the formula in a local closure, which proves
        // the copy and would have passed even if the production arithmetic were
        // deleted outright.
        assert_eq!(
            ring_bytes_for_ram(Some(4 * GIB)),
            FRAME_RING_MAX_BYTES,
            "4 GiB -> floor"
        );
        assert!(
            ring_bytes_for_ram(Some(32 * GIB)) > FRAME_RING_MAX_BYTES,
            "a 32 GiB host must get MORE than the 4 GiB-era floor — that gap is \
             the entire reason this function exists"
        );
        assert!(
            ring_bytes_for_ram(Some(32 * GIB)) <= ring_bytes_for_ram(Some(64 * GIB)),
            "monotonic"
        );
        assert_eq!(
            ring_bytes_for_ram(Some(1024 * GIB)),
            FRAME_RING_MAX_BYTES_CEILING,
            "an enormous host is capped, not unbounded"
        );
    }

    // -----------------------------------------------------------------------
    // Extreme permutations of the sizing input
    // -----------------------------------------------------------------------

    /// The bounds must hold for EVERY input, not the handful a real machine
    /// reports.
    ///
    /// The sizing function is the one place in this lane where an outside
    /// number (a file the kernel writes) decides how much memory the process
    /// takes. Anything that number can be, it will eventually be — on a
    /// container with a synthetic `/proc`, on a host with an absurd
    /// hugepage-backed total, on a kernel that changes its mind about units.
    /// So the guarantee has to be range-wide, not sample-wide.
    #[test]
    fn test_ring_bytes_stays_inside_its_bounds_for_every_conceivable_ram() {
        let mut cases: Vec<Option<usize>> = vec![
            None,             // unreadable /proc/meminfo
            Some(0),          // a kernel reporting zero
            Some(1),          // one byte
            Some(99),         // below the /100 divisor — integer division to zero
            Some(100),        // exactly the divisor
            Some(101),        // just above
            Some(usize::MAX), // the overflow edge
            Some(usize::MAX / 2),
        ];
        // Every power of two from 1 byte to the top of the type: catches a
        // shift/rounding error at any scale, not just the plausible ones.
        for shift in 0..usize::BITS {
            cases.push(Some(1usize << shift));
        }
        // And every whole GiB from 1 to 4096 — the range a real box lives in.
        for gib in 1..=4096usize {
            cases.push(Some(gib * GIB));
        }

        for input in cases {
            let out = ring_bytes_for_ram(input);
            assert!(
                out >= FRAME_RING_MAX_BYTES,
                "ring_bytes_for_ram({input:?}) = {out} fell BELOW the proven \
                 floor. The floor is what makes auto-sizing unable to regress a \
                 working configuration; below it this change becomes a \
                 tick-loss risk instead of a headroom gain"
            );
            assert!(
                out <= FRAME_RING_MAX_BYTES_CEILING,
                "ring_bytes_for_ram({input:?}) = {out} exceeded the ceiling — \
                 the ring would start competing with QuestDB for the same RAM"
            );
        }
    }

    #[test]
    fn test_ring_sizing_never_panics_and_never_wraps_at_the_type_edge() {
        // `usize::MAX / 100 * 2` overflows in debug builds without the
        // saturating multiply, and WRAPS to a tiny number in release — which
        // would clamp back to the floor and look perfectly healthy while the
        // arithmetic was silently broken. Pin both directions.
        assert_eq!(
            ring_bytes_for_ram(Some(usize::MAX)),
            FRAME_RING_MAX_BYTES_CEILING,
            "the largest representable host must saturate to the ceiling, not \
             wrap into the floor"
        );
        assert_eq!(
            ring_bytes_for_ram(Some(0)),
            FRAME_RING_MAX_BYTES,
            "a zero-RAM reading must land on the floor"
        );
    }

    #[test]
    fn test_ring_sizing_is_monotonic_across_the_whole_gib_range() {
        // Monotonicity at three sample points is a spot check. Across the full
        // range it is a property: more RAM can never produce a smaller ring, at
        // any boundary, including the two clamp edges where an off-by-one would
        // otherwise hide.
        let mut prev = ring_bytes_for_ram(Some(0));
        for gib in 1..=512usize {
            let now = ring_bytes_for_ram(Some(gib * GIB));
            assert!(
                now >= prev,
                "sizing went DOWN between {} and {gib} GiB ({prev} -> {now})",
                gib - 1
            );
            prev = now;
        }
    }

    #[test]
    fn test_the_percentage_actually_binds_somewhere_in_the_real_range() {
        // Non-vacuity of the whole feature. If the floor and ceiling were set
        // such that the percentage never decided anything, every bounds test
        // above would still pass and the function would be an elaborate way of
        // returning a constant.
        let strictly_between = (1..=256usize)
            .map(|gib| ring_bytes_for_ram(Some(gib * GIB)))
            .filter(|&b| b > FRAME_RING_MAX_BYTES && b < FRAME_RING_MAX_BYTES_CEILING)
            .count();
        assert!(
            strictly_between > 0,
            "no host size between 1 and 256 GiB produces a ring strictly \
             between the floor and the ceiling — the percentage never binds, \
             so auto-sizing is decorative"
        );
    }

    // -----------------------------------------------------------------------
    // Extreme permutations of the /proc/meminfo TEXT
    // -----------------------------------------------------------------------

    #[test]
    fn test_meminfo_parser_accepts_the_real_shape() {
        let real = "MemTotal:       32819128 kB\nMemFree:         1234567 kB\n";
        assert_eq!(
            parse_meminfo_total_bytes(real),
            Some(32_819_128 * 1024),
            "the ordinary Linux shape must parse"
        );
    }

    #[test]
    fn test_meminfo_parser_refuses_every_malformed_shape() {
        // Each of these once looked like "obviously fine" input. The refusal is
        // the point: an unparseable file falls back to the proven floor, which
        // is a correct system running with less headroom — never a guess.
        let hostile: &[(&str, &str)] = &[
            ("", "empty file"),
            ("\n\n\n", "blank lines only"),
            ("MemFree: 100 kB\n", "no MemTotal line at all"),
            ("MemTotal:\n", "key with no value"),
            ("MemTotal:       \n", "key with whitespace only"),
            ("MemTotal: notanumber kB\n", "non-numeric value"),
            ("MemTotal: -1 kB\n", "negative value"),
            ("MemTotal: 3.5 kB\n", "fractional value"),
            (
                "MemTotal: 32819128\n",
                "value with NO unit — the 1024x trap",
            ),
            ("MemTotal: 32819128 MB\n", "wrong unit MB"),
            ("MemTotal: 32819128 B\n", "wrong unit bytes"),
            ("MemTotal: 32819128 kb\n", "wrong case — kernel writes kB"),
            ("memtotal: 32819128 kB\n", "lowercase key"),
            (
                "MemTotalSwap: 5 kB\n",
                "a DIFFERENT key that shares the prefix",
            ),
            (" MemTotal: 5 kB\n", "leading space breaks the line anchor"),
            (
                "MemTotal: 99999999999999999999999999 kB\n",
                "value beyond usize — must not wrap",
            ),
        ];
        for (input, why) in hostile {
            assert_eq!(
                parse_meminfo_total_bytes(input),
                None,
                "parser ACCEPTED malformed input ({why}): {input:?}. Accepting \
                 it would size a live buffer from a number nobody validated"
            );
        }
    }

    #[test]
    fn test_meminfo_parser_takes_the_first_memtotal_when_duplicated() {
        // The kernel writes one. A synthetic /proc (containers, test harnesses,
        // a hostile mount) can write several. Pick deterministically rather
        // than inventing a merge rule that nobody can predict.
        let dup = "MemTotal: 1000 kB\nMemTotal: 9999999 kB\n";
        assert_eq!(parse_meminfo_total_bytes(dup), Some(1000 * 1024));
    }

    #[test]
    fn test_meminfo_parser_tolerates_odd_but_valid_whitespace() {
        // These are NOT malformed — the kernel's column alignment varies with
        // the value width, so the parser must not be brittle about spacing.
        for ok in [
            "MemTotal: 4194304 kB\n",
            "MemTotal:4194304 kB\n",
            "MemTotal:\t4194304\tkB\n",
            "MemTotal:          4194304    kB",
            "MemFree: 1 kB\nMemTotal: 4194304 kB\nSwapTotal: 0 kB\n",
        ] {
            assert_eq!(
                parse_meminfo_total_bytes(ok),
                Some(4_194_304 * 1024),
                "parser rejected a VALID kernel spacing variant: {ok:?}. A \
                 false rejection is not harmless — it silently drops the box \
                 back to the 4 GiB-era floor"
            );
        }
    }

    #[test]
    fn test_a_refused_meminfo_lands_exactly_on_todays_behaviour() {
        // The end-to-end safety claim, stated as one assertion: every way the
        // parse can fail produces the byte budget that shipped before this
        // feature existed. That is what makes the change unable to make
        // anything worse.
        for broken in ["", "garbage", "MemTotal: x kB", "MemTotal: 1"] {
            assert_eq!(
                ring_bytes_for_ram(parse_meminfo_total_bytes(broken)),
                FRAME_RING_MAX_BYTES,
                "a broken meminfo ({broken:?}) must land on the pre-existing \
                 constant, not on a guess"
            );
        }
    }
}

#[cfg(test)]
mod silence_latch_tests {
    use super::silence_page_is_cooling;

    /// RISK-GAP-03 must be rate-limited between PAGES, not just per episode.
    ///
    /// # What the 2026-08-14 session actually did
    ///
    /// 25 distinct emits in one trading day. The `silent` count oscillated
    /// throughout — 4, 9, 1, 2, 1, 3, 208, 10 — clearing to zero between
    /// episodes and re-arming the per-episode latch each time, entirely
    /// legitimately: sparse-cadence instruments go quiet and come back.
    ///
    /// So the latch worked exactly as designed and still produced ~25 pages,
    /// because the world produced ~25 episodes. `never_ticked` was 4 on the
    /// 09:15 emit and 0 on every one after it, so the feed WAS delivering.
    ///
    /// That became a paging problem on 2026-08-15, when RISK-GAP-03 gained a
    /// CloudWatch alarm: 25 episodes at a 5-minute window with a recovery page
    /// is ~50 operator messages a day, which is how a pager gets ignored — and
    /// this one is the only signal that exists for a subscribe that silently
    /// did not take.
    ///
    /// A source scan because reproducing it behaviourally needs a live socket,
    /// a seeded universe and a session of wall clock; the emit condition is
    /// one expression and its shape is the whole fix.
    #[test]
    fn test_risk_gap_03_page_is_rate_limited_across_episodes() {
        let src = include_str!("dhan_feed_stack.rs");
        let test_marker = concat!("#[cfg(", "test)]");
        let production = src.split(test_marker).next().unwrap_or(src);

        assert!(
            production.contains("!silence_reported && !cooling"),
            "the RISK-GAP-03 emit is gated only by the per-episode latch. The \
             2026-08-14 session had ~25 legitimate episodes, so a per-episode \
             latch pages ~25 times — and this code now drives a CloudWatch alarm"
        );
        assert!(
            production.contains("SILENCE_PAGE_COOLDOWN_SECS: u64 = 1_800"),
            "the page cooldown must sit above the observed inter-episode gap \
             (two to five minutes on 2026-08-14) and far below a session, so a \
             genuinely new problem hours later still pages"
        );

        // The suppressed episodes must stay COUNTABLE. Rate-limiting the page
        // while also gating the gauge would trade a noisy signal for no signal.
        let scan_arm = production
            .split("ingest.scan_silence_named(now_millis, &mut worst_silent);")
            .nth(1)
            .unwrap_or_default();
        let gauge = scan_arm
            .find("tv_dhan_feed_instruments_silent")
            .expect("the silent gauge must publish from the scan arm");
        let cooldown = scan_arm.find("!cooling").unwrap_or(usize::MAX);
        assert!(
            gauge < cooldown,
            "the silent gauge publishes after the page gate, so a rate-limited \
             episode would go uncounted as well as unpaged"
        );
    }

    /// The first page of a session is never suppressed.
    #[test]
    fn test_the_first_silence_page_of_a_session_always_fires() {
        assert!(
            !silence_page_is_cooling(None, 34_200, 1_800),
            "no page has fired yet, so nothing can be cooling — suppressing \
             the first page would lose the 09:15 emit entirely"
        );
    }

    /// The observed inter-episode gap must be suppressed; a later hour must not.
    #[test]
    fn test_the_cooldown_spans_the_observed_episode_gap_but_not_the_session() {
        let first = 46_800; // 13:00 IST, in seconds of day

        // 2026-08-14 re-fired every two to five minutes through the
        // afternoon. Every one of those must fold into the first page.
        for gap in [120, 180, 300, 600, 1_799] {
            assert!(
                silence_page_is_cooling(Some(first), first + gap, 1_800),
                "a re-fire {gap}s later still pages. The 2026-08-14 session \
                 had ~25 legitimate episodes at exactly these gaps, which is \
                 ~50 operator messages once the CloudWatch alarm is attached"
            );
        }

        // A genuinely new problem later must still reach the operator.
        for gap in [1_800, 3_600, 7_200] {
            assert!(
                !silence_page_is_cooling(Some(first), first + gap, 1_800),
                "a fresh episode {gap}s later is suppressed. The cooldown \
                 must bound noise, not blind the rest of the session"
            );
        }
    }

    /// A counter-shaped value must not quietly behave like a clock.
    ///
    /// The call site sits a few lines below a binding named `now` holding
    /// `ingest.refusals()`. Passing that instead of the clock compiles. This
    /// pins what it would do: with a small refusal count the cooldown is
    /// permanently active after the first page — every later episode silently
    /// suppressed for the rest of the session.
    #[test]
    fn test_a_counter_shaped_value_does_not_silently_work_as_a_clock() {
        // A refusal counter that never moves: every subsequent call sees the
        // same "time", so elapsed is 0 and the cooldown never expires.
        let refusals_as_secs = 0_u64;
        assert!(
            silence_page_is_cooling(Some(refusals_as_secs), refusals_as_secs, 1_800),
            "a frozen counter must read as still-cooling — this is the \
             failure mode, recorded so the guard above has teeth"
        );

        // And with the real clock the same elapsed span does expire, which is
        // the difference the call site has to get right.
        let t = 46_800_u64;
        assert!(!silence_page_is_cooling(Some(t), t + 1_800, 1_800));
    }

    /// A clock stepping backwards must not clear the cooldown.
    #[test]
    fn test_a_backwards_clock_step_does_not_clear_the_cooldown() {
        let first = 46_800;
        assert!(
            silence_page_is_cooling(Some(first), first - 5, 1_800),
            "an NTP correction that steps the clock back must not wrap into a \
             huge elapsed value and release the cooldown at the exact moment \
             the logs are hardest to read"
        );
    }

    #[test]
    fn test_the_risk_gap_03_alarm_window_absorbs_oscillation() {
        let tf = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../deploy/aws/terraform/error-code-alarms.tf"),
        )
        .expect("error-code-alarms.tf must be readable");

        let entry = tf
            .split("\"risk-gap-03\" = {")
            .nth(1)
            .expect("the risk-gap-03 alarm entry must exist");
        // Bound on `desc`, not on the first `}` — the `pattern` attribute is a
        // CloudWatch filter expression that contains braces of its own, so a
        // brace scan stops before the attributes this test is about. A first
        // draft did exactly that and reported the values missing when they
        // were present three lines further down.
        let entry = &entry[..entry.find("desc").unwrap_or(entry.len())];

        assert!(
            entry.contains("period      = 3600"),
            "the risk-gap-03 alarm window shrank below an hour. A 5-minute \
             window flaps on the residual oscillation and pages on every edge \
             — the 2026-08-14 session would have produced ~25 transitions"
        );
        assert!(
            entry.contains("ok_recovery = false"),
            "the risk-gap-03 alarm sends a recovery page. It cannot tell 'the \
             feed is healthy again' from 'one sparse contract traded once', so \
             an OK here reads as the first while meaning the second"
        );
    }
}

#[cfg(test)]
mod contract_attach_tests {
    use super::{DEPTH_ATTACH_PREOPEN_RETRY_SECS, DEPTH_ATTACH_RETRY_SECS, preopen_retry_secs};

    /// The four boundaries that decide whether the operator's 09:12 line is
    /// met. Asserted as a table because an off-by-one at either edge is
    /// invisible in production: the loop simply runs at the other cadence and
    /// nothing logs which one it chose.
    /// The deadline must fall BEFORE the minute the operator names as ready,
    /// and after ticks start persisting — a deadline earlier than 09:00 could
    /// never be met because no price exists to locate at-the-money against.
    #[test]
    fn preopen_ready_deadline_sits_between_first_tick_and_the_open() {
        use super::PREOPEN_READY_DEADLINE_IST_SECS as D;
        let persist = tickvault_common::constants::TICK_PERSIST_START_SECS_OF_DAY_IST;
        assert!(
            D > persist,
            "a deadline before the first tick can never be met"
        );
        assert_eq!(D, 9 * 3_600 + 12 * 60, "the deadline is 09:12:00 IST");
        assert!(
            u64::from(D) < super::CONTINUOUS_SESSION_START_SECS_OF_DAY_IST,
            "readiness must precede the open, not coincide with it"
        );
        // Inside the fast-poll window, or the deadline would be approached on
        // the 60s grid it exists to defeat.
        assert_eq!(
            super::preopen_retry_secs(D),
            super::DEPTH_ATTACH_PREOPEN_RETRY_SECS,
            "the deadline must be approached at the fast cadence"
        );
    }

    /// THE test for this change. `out_of_time` drives two arms — the quorum
    /// wait and the GIVE-UP arm — and only the first wants a 09:12 deadline.
    /// Folding it in would abandon contracts and depth at 09:12 on a morning
    /// where nothing had resolved, which is the opposite of the requirement.
    ///
    /// A source scan because the loop is `TEST-EXEMPT` async I/O: it dials
    /// sockets. This asserts the exact structural property that keeps the two
    /// meanings apart, which no unit test of a pure function can reach.
    #[test]
    fn preopen_ready_deadline_never_reaches_the_give_up_arm() {
        // PRODUCTION half only — `include_str!` carries this test module too,
        // and the first draft counted its own assertion text (5 instead of 2).
        // The house `test_marker` split is what keeps a source scan measuring
        // the code rather than measuring itself.
        let full = include_str!("dhan_feed_stack.rs");
        let test_marker = concat!("#[cfg(", "test)]");
        let src = full.split(test_marker).next().unwrap_or(full);
        assert!(
            src.contains("let out_of_time = past_hard_stop || past_deadline_and_window;"),
            "out_of_time gained a term — if that term is the 09:12 deadline, the give-up \
             arm now abandons the session at 09:12"
        );
        assert!(
            src.contains(
                "let past_preopen_ready_deadline = now_ist >= PREOPEN_READY_DEADLINE_IST_SECS;"
            ),
            "the readiness deadline must stay its own flag"
        );
        // And it must be spent on the quorum wait, nowhere else.
        assert_eq!(
            src.matches("past_preopen_ready_deadline").count(),
            2,
            "the readiness flag is declared once and read once (the quorum wait). A third \
             use means it has reached an arm that was never reviewed for it"
        );
        assert!(
            src.contains("&& !past_preopen_ready_deadline;"),
            "the quorum wait must actually consult the deadline"
        );
    }

    /// A mid-session restart is not a late pre-open, and must not be reported
    /// as one.
    ///
    /// MEASURED 2026-08-25: the morning attach completed at 09:08:04 and MET
    /// the 09:12 deadline, and five afternoon restarts each fired `WS-GAP-02`
    /// and drove `tv-<env>-preopen-ready-late` into ALARM. The alarm is
    /// ungated by design, so it stayed armed. Every one of those pages was
    /// reporting a redeploy.
    #[test]
    fn the_preopen_deadline_does_not_apply_to_a_mid_session_restart() {
        // The real morning: the loop began well before the deadline.
        assert!(
            super::preopen_deadline_applies(8 * 3_600 + 35 * 60),
            "an attach that begins at 08:35 is racing the open and IS judged"
        );
        // The five real restarts from that afternoon, by their start second.
        for (label, started) in [
            ("12:37", 12 * 3_600 + 37 * 60),
            ("16:17", 16 * 3_600 + 17 * 60),
            ("17:33", 17 * 3_600 + 33 * 60),
            ("18:17", 18 * 3_600 + 17 * 60),
            ("19:21", 19 * 3_600 + 21 * 60),
        ] {
            assert!(
                !super::preopen_deadline_applies(started),
                "the {label} restart began after 09:12 and cannot be judged against it"
            );
        }
    }

    /// Keyed on the START, never the finish.
    ///
    /// An attach that begins at 09:11 and drags past 09:12 is the exact case
    /// the deadline exists for; excusing it would hollow the alarm out from
    /// the other side while fixing the false pages.
    #[test]
    fn an_attach_that_begins_before_the_deadline_is_still_judged_if_it_overruns() {
        let d = super::PREOPEN_READY_DEADLINE_IST_SECS;
        assert!(
            super::preopen_deadline_applies(d - 1),
            "one second before the deadline still counts as racing it"
        );
        assert!(
            !super::preopen_deadline_applies(d),
            "an attach that begins AT the deadline has already lost the race it would be \
             judged on — it is a restart, not a late pre-open"
        );
        assert!(!super::preopen_deadline_applies(d + 1));
    }

    /// The alarm-side half of the fix, pinned in source.
    ///
    /// The CloudWatch metric filter is `{ $.fields.ready_at_ist_secs = * }`.
    /// The mid-session arm must therefore NOT carry that field name, or the
    /// datapoint still lands and the gate changes nothing that the operator
    /// can see. Bite-proven: renaming `attached_at_ist_secs` back to
    /// `ready_at_ist_secs` fails this test.
    #[test]
    fn the_mid_session_arm_does_not_emit_the_field_the_alarm_filters_on() {
        let full = include_str!("dhan_feed_stack.rs");
        let test_marker = concat!("#[cfg(", "test)]");
        let src = full.split(test_marker).next().unwrap_or(full);

        let gate = src
            .find("if !preopen_deadline_applies(attach_started_ist) {")
            .expect("the mid-session gate must exist");
        let els = src[gate..]
            .find("} else {")
            .expect("the judged arm must be the else of that gate")
            + gate;
        // Comment lines are stripped before asserting: this arm's own comment
        // QUOTES the CloudWatch filter pattern verbatim, so a raw substring
        // scan matches the documentation rather than the code. Caught by this
        // test failing on its first run, which is the right way round.
        let mid_arm: String = src[gate..els]
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        let mid_arm = mid_arm.as_str();

        assert!(
            mid_arm.contains("attached_at_ist_secs = ready_at"),
            "the mid-session arm must report its completion second under a DIFFERENT field"
        );
        assert!(
            !mid_arm.contains("ready_at_ist_secs ="),
            "the mid-session arm must not emit `ready_at_ist_secs` — that field is what the \
             CloudWatch filter matches, so emitting it re-creates the false page"
        );
        assert!(!mid_arm.contains("error!"), "a restart is not an error");
        assert!(
            !mid_arm.contains("metrics::gauge!(PREOPEN_READY_GAUGE)"),
            "there is no readiness second on a restart; publishing the wall clock as one \
             would read as a permanently missed deadline"
        );
    }

    /// The readiness second is published once, at the only point where both
    /// halves are provably on the wire — never on a give-up path, where there
    /// is no readiness second and a placeholder would read as a met deadline.
    #[test]
    fn preopen_ready_gauge_is_published_only_on_the_both_halves_success_arm() {
        let full = include_str!("dhan_feed_stack.rs");
        let test_marker = concat!("#[cfg(", "test)]");
        let src = full.split(test_marker).next().unwrap_or(full);
        assert_eq!(
            src.matches("metrics::gauge!(PREOPEN_READY_GAUGE)").count(),
            1,
            "the readiness gauge must have exactly one emit site"
        );
        let emit = src
            .find("metrics::gauge!(PREOPEN_READY_GAUGE)")
            .expect("emit site");
        // The arm gained a `!readiness_published` latch on 2026-08-22: the
        // loop can now outlive the both-halves moment to chase stocks that
        // priced after the contract dial, so the `return` no longer guarantees
        // one publication per session. The latch does. Anchoring on the FULL
        // condition is deliberate — anchoring on the prefix would silently
        // match the terminal return block further down and invert this
        // assertion, which is exactly how this test first failed.
        let arm = src
            .find("if contracts_done && depth_done && !readiness_published {")
            .expect("success arm");
        assert!(emit > arm, "the gauge is published outside the success arm");
        assert_eq!(
            src.matches("readiness_published = true;").count(),
            1,
            "one latch set, inside that arm — a second would let a lingering loop republish \
             the readiness second and overwrite a met deadline with a later one"
        );
        // Measured AFTER the dials, not from the top of the iteration — the
        // dials take real time and the earlier reading would flatter it.
        assert!(
            src.contains("let ready_at = ist_second_of_day_now();"),
            "the readiness second must be read fresh at completion"
        );
    }

    #[test]
    fn preopen_retry_secs_switches_exactly_at_the_persistence_and_session_edges() {
        const NINE: u32 = 9 * 3_600;
        for (secs, want, why) in [
            (
                NINE - 1,
                DEPTH_ATTACH_RETRY_SECS,
                "08:59:59 — before ticks persist",
            ),
            (
                NINE,
                DEPTH_ATTACH_PREOPEN_RETRY_SECS,
                "09:00:00 — first tick can persist",
            ),
            (
                NINE + 8 * 60,
                DEPTH_ATTACH_PREOPEN_RETRY_SECS,
                "09:08 — Friday's real attach",
            ),
            (
                NINE + 12 * 60,
                DEPTH_ATTACH_PREOPEN_RETRY_SECS,
                "09:12 — the operator's line",
            ),
            (
                NINE + 15 * 60 - 1,
                DEPTH_ATTACH_PREOPEN_RETRY_SECS,
                "09:14:59 — still pre-open",
            ),
            (
                NINE + 15 * 60,
                DEPTH_ATTACH_RETRY_SECS,
                "09:15:00 — open; nothing races now",
            ),
            (
                14 * 3_600,
                DEPTH_ATTACH_RETRY_SECS,
                "14:00 — mid-session restart",
            ),
            (
                0,
                DEPTH_ATTACH_RETRY_SECS,
                "midnight — no fast poll overnight",
            ),
        ] {
            assert_eq!(preopen_retry_secs(secs), want, "{why}");
        }
    }

    /// The fast cadence must be strictly faster, and bounded away from zero —
    /// a 0 would spin the loop against QuestDB with no sleep at all.
    #[test]
    fn preopen_retry_secs_is_faster_than_the_session_cadence_and_never_zero() {
        assert!(
            DEPTH_ATTACH_PREOPEN_RETRY_SECS > 0,
            "a zero sleep is a spin loop"
        );
        assert!(
            DEPTH_ATTACH_PREOPEN_RETRY_SECS < DEPTH_ATTACH_RETRY_SECS,
            "the pre-open cadence must be the faster one, or the window buys nothing"
        );
        // 15 minutes of window / 5s = 180 reads. Pinned so a later tightening
        // has to face the read count it creates.
        let reads = (15 * 60) / DEPTH_ATTACH_PREOPEN_RETRY_SECS;
        assert!(
            reads <= 200,
            "pre-open window would issue {reads} QuestDB reads"
        );
    }

    use super::*;

    #[test]
    fn remaining_capacity_is_counted_in_whole_connections() {
        // The spot universe is ~4,565 instruments — one connection carrying
        // 4,565 of its 5,000. The 435 unused slots on that socket are
        // unreachable to a second planning pass, so the contract set gets 4
        // connections, not 20,435 instruments.
        assert_eq!(remaining_main_feed_capacity(1), 20_000);
        assert_eq!(remaining_main_feed_capacity(0), 25_000);
        assert_eq!(remaining_main_feed_capacity(4), 5_000);
    }

    #[test]
    fn a_full_pool_leaves_no_room_rather_than_overflowing_it() {
        // Every connection spent means NO contracts. Returning anything
        // non-zero here would size a set the pool cannot dial, and `plan_pool`
        // refuses the WHOLE pool — costing the session its depth as well.
        assert_eq!(remaining_main_feed_capacity(5), 0);
        assert_eq!(
            remaining_main_feed_capacity(9),
            0,
            "saturating, never wraps"
        );
    }

    #[test]
    fn connection_count_mirrors_the_main_feed_pack_policy() {
        // History, because this line has now been wrong in BOTH directions and
        // the reason is the same each time: it must mirror `plan_pool`'s
        // main-feed arm, and nothing else.
        //
        //  - originally PACKED while plan_pool SPREAD  -> understated by 4,
        //    the attach asked for room that did not exist, the pool refused
        //    everything and depth was never planned;
        //  - briefly SPREAD to match                   -> honest, but left
        //    literally zero room for contracts;
        //  - now PACKED, with plan_pool packing too    -> both passes fit.
        let max = usize::from(tickvault_core::websocket::pool_budget::MAX_MAIN_FEED_CONNECTIONS);
        let cap = usize::try_from(
            tickvault_core::websocket::pool_budget::MAIN_FEED_INSTRUMENTS_PER_CONNECTION,
        )
        .unwrap_or(usize::MAX);

        assert_eq!(
            main_feed_connections_for(0, max),
            0,
            "nothing occupies nothing"
        );
        assert_eq!(main_feed_connections_for(1, max), 1);
        assert_eq!(main_feed_connections_for(3, max), 1, "three fit one socket");
        assert_eq!(
            main_feed_connections_for(cap, max),
            1,
            "exactly full is still one"
        );
        assert_eq!(
            main_feed_connections_for(cap + 1, max),
            2,
            "one over spills"
        );
        assert_eq!(
            main_feed_connections_for(4_565, max),
            1,
            "today's spot universe fits one socket — leaving four for contracts"
        );
        assert_eq!(main_feed_connections_for(25_000, max), max, "target scale");

        // The clamp. Reporting more than the pool has free is what earns a
        // BudgetRefused on the WHOLE pool.
        assert_eq!(main_feed_connections_for(25_000, 2), 2);
        assert_eq!(main_feed_connections_for(25_000, 0), 0);
    }

    #[test]
    fn the_two_helpers_can_never_together_exceed_the_authorized_pool() {
        // The 5-connection main-feed lock has to be arithmetic, not a hope:
        // for every reachable spot-universe size, the connections the spots
        // occupy plus the connections the remaining capacity could need must
        // stay inside the cap.
        let max = usize::from(tickvault_core::websocket::pool_budget::MAX_MAIN_FEED_CONNECTIONS);
        for spot in [0usize, 1, 3, 4_565, 5_000, 5_001, 12_000, 24_999, 25_000] {
            let used = main_feed_connections_for(spot, max);
            let remaining = remaining_main_feed_capacity(used);
            // Contracts may only occupy what the spots LEFT.
            let would_need = main_feed_connections_for(remaining, max.saturating_sub(used));
            assert!(
                used + would_need <= max,
                "spot={spot} used={used} + contracts={would_need} exceeds {max}"
            );
        }

        // And the property the whole fix exists for, stated directly: after
        // today's spot universe, contracts still fit — and together they fill
        // every authorized main-feed socket. Under the old arithmetic this
        // line read 0 connections for contracts and a refused pool.
        let used = main_feed_connections_for(4_565, max);
        assert_eq!(used, 1);
        let room = remaining_main_feed_capacity(used);
        assert_eq!(room, 20_000, "four connections x 5,000 remain");
        assert_eq!(
            used + main_feed_connections_for(room, max.saturating_sub(used)),
            max,
            "spots + contracts together fill all five main-feed sockets"
        );
    }

    #[test]
    fn ymd_from_ist_date_refuses_a_malformed_date_rather_than_selecting_an_expired_one() {
        assert_eq!(ymd_from_ist_date("2026-08-19"), 2026_08_19);
        // Every refusal returns 0, and 0 makes every expiry compare as "before
        // today" — so the fail-closed direction is "no contracts", never "an
        // expired one", whose silence looks exactly like a quiet book.
        assert_eq!(ymd_from_ist_date(""), 0);
        assert_eq!(ymd_from_ist_date("19-08-2026"), 0);
        assert_eq!(ymd_from_ist_date("2026-13-01"), 0);
        assert_eq!(ymd_from_ist_date("2026-08-32"), 0);
        assert_eq!(ymd_from_ist_date("not-a-date"), 0);
    }

    #[test]
    fn the_date_packing_matches_the_one_expiries_are_stored_in() {
        // Both sides of the expiry comparison must pack identically or the
        // whole >= today filter is meaningless. Same input, same answer.
        for d in ["2026-08-19", "2026-01-01", "2026-12-31"] {
            assert_eq!(
                ymd_from_ist_date(d),
                tickvault_core::instrument::master_csv::parse_expiry_ymd(d),
                "the attach and the parser must agree on {d}"
            );
        }
    }
}

#[cfg(test)]
mod alive_connection_guard_tests {
    use super::{ALIVE_CONNECTIONS, AliveConnectionGuard};
    use std::sync::atomic::Ordering;
    use std::sync::{Mutex, MutexGuard, OnceLock};

    /// Serializes every test in this module.
    ///
    /// `ALIVE_CONNECTIONS` is process-global and these tests read EXACT values
    /// around it (`base`, `base + 1`), so two of them on parallel threads see
    /// each other's increments and fail for a reason unrelated to what they
    /// check. `cargo test` shares one process across threads, so the race is
    /// real there; nextest gives each test its own process, which is why CI
    /// never showed it.
    ///
    /// Fixed independently on `main` the same day with an inline
    /// `static SERIALIZE: Mutex<()>`; the two are equivalent and this side's
    /// helper is kept because the test bodies here already call it. The same
    /// class was fixed the same way in `tv_api_token_prod_guard.rs` after
    /// PR #1411 merged red on it (merge-gate-lock section 3.2).
    ///
    /// This replaces a comment that CLAIMED the tests were "serialized ... by
    /// running both assertions here" — true when this module held one test,
    /// and quietly false from the moment a second one was added beside it.
    /// Poisoning is recovered rather than propagated: a panic inside one test
    /// (which one of them raises deliberately) must not convert every other
    /// test in the module into a spurious failure.
    fn serial() -> MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
    #[test]
    fn the_count_comes_back_down_on_a_panic_not_only_on_a_clean_return() {
        let _serial = serial();
        // The defect this exists for: the increment happens OUTSIDE the socket
        // task, so a decrement written as a plain statement at the end of the
        // task body is skipped when the body unwinds. The gauge then reports N
        // sockets alive when N-1 are — permanently, with nothing to correct
        // it, on the one number an operator reads to answer "how many of the
        // sixteen are up?".
        //
        // CORRECTED 2026-08-21: this comment used to claim the module was
        // "serialized ... by running both assertions here". It was not — the
        // second test below mutates the same global, so the two raced, and the
        // comment asserting safety is exactly what stopped anyone looking.
        // The real serialization is the `serial()` guard taken above.
        let base = ALIVE_CONNECTIONS.load(Ordering::SeqCst);

        // Clean path.
        let g = AliveConnectionGuard::acquire();
        assert_eq!(ALIVE_CONNECTIONS.load(Ordering::SeqCst), base + 1);
        assert_eq!(g.release(), base);
        assert_eq!(ALIVE_CONNECTIONS.load(Ordering::SeqCst), base);

        // Unwind path — the one a plain decrement misses.
        let unwound = std::panic::catch_unwind(|| {
            let _g = AliveConnectionGuard::acquire();
            assert_eq!(ALIVE_CONNECTIONS.load(Ordering::SeqCst), base + 1);
            panic!("socket task died");
        });
        assert!(unwound.is_err(), "the panic must actually have happened");
        assert_eq!(
            ALIVE_CONNECTIONS.load(Ordering::SeqCst),
            base,
            "an unwound socket task must still give its slot back — otherwise \
             the alive gauge is overstated for the rest of the process"
        );
    }

    #[test]
    fn release_disarms_so_drop_cannot_double_count() {
        let _serial = serial();
        // `release` consumes the guard and `Drop` still runs on the moved-out
        // value. If it did not disarm, every clean exit would decrement twice
        // and the gauge would read LOW — the opposite failure, equally wrong,
        // and the one a naive guard introduces while fixing the first.
        let base = ALIVE_CONNECTIONS.load(Ordering::SeqCst);
        let g = AliveConnectionGuard::acquire();
        let remaining = g.release();
        assert_eq!(remaining, base);
        assert_eq!(
            ALIVE_CONNECTIONS.load(Ordering::SeqCst),
            base,
            "exactly one decrement per acquire"
        );
    }
}

#[cfg(test)]
mod inline_depth_tests {
    use super::*;

    #[test]
    fn with_inline_depth_is_off_by_default_and_the_builder_enables_it() {
        // The default must stay OFF. At the 25,000 target this path writes
        // ~611M rows/day; switching it on for every existing caller by
        // accident is precisely the wrong default.
        let ingest = LiveIngest::new(TickWriter::for_test(Feed::Dhan), 8);
        assert!(
            ingest.inline_depth.is_none(),
            "inline depth must be OFF by default"
        );
        let ingest = ingest.with_inline_depth(DepthIngest::for_test());
        assert!(ingest.inline_depth.is_some(), "the builder must enable it");
    }

    #[test]
    fn append_inline_depth_writes_both_sides_of_every_level() {
        // 5 levels x 2 sides = 10 rows.
        //
        // CORRECTED 2026-08-20: this comment used to justify a PER-SIDE
        // capture_seq ("each side of a level gets its own ... a shared seq
        // would let the DEDUP key collapse them"). That reasoning was wrong in
        // both directions. `side` and `level` are THEMSELVES DEDUP key columns,
        // so the ten rows of one packet were never at risk of collapsing; and
        // encoding the level/side ordinal there consumed the one column that
        // could carry the PACKET's identity, which is what actually collides.
        // See `append_inline_depth`'s own docs.
        let mut sink = DepthIngest::for_test();
        let tick = tickvault_common::tick_types::ParsedTick {
            security_id: 13,
            exchange_segment_code: 0,
            ..Default::default()
        };
        let levels = [tickvault_common::tick_types::MarketDepthLevel {
            bid_quantity: 10,
            ask_quantity: 20,
            bid_orders: 1,
            ask_orders: 2,
            bid_price: 100.5,
            ask_price: 100.75,
        }; 5];
        let rows = append_inline_depth(
            &mut sink,
            &tick,
            &levels,
            1_700_000_000_000_000_000,
            0,
            0,
            counters(),
        );
        assert_eq!(rows, 10, "5 levels x 2 sides");
    }

    #[test]
    fn append_inline_depth_writes_clean_decimals_not_widening_artifacts() {
        // The defect this pins, live for a day before it was caught: the
        // inline 5-level book carries Dhan's f32 prices, and this writer
        // widened them with a bare `f64::from`. 10.20 went into the
        // `market_depth` table as 10.199999809265137 and 23925.65 as
        // 23925.650390625 — while the depth-20/200 rows for the SAME
        // instrument at the SAME instant, written a few hundred lines away
        // from native-f64 wire values, carried the exact decimals.
        //
        // One table, two precisions, and no counter that could show it: `rows`
        // counts ILP appends, not what the numbers say. Any price equality
        // against `ticks` (which has always applied this conversion) silently
        // missed, and option premiums sit on x.05/x.20/x.35 constantly, so it
        // bit nearly every level rather than rarely.
        //
        // Bite-proven: reverting to `f64::from` fails on the first assert.
        let mut sink = DepthIngest::for_test();
        let tick = tickvault_common::tick_types::ParsedTick {
            security_id: 13,
            exchange_segment_code: 0,
            ..Default::default()
        };
        let levels = [tickvault_common::tick_types::MarketDepthLevel {
            bid_quantity: 10,
            ask_quantity: 20,
            bid_orders: 1,
            ask_orders: 2,
            // Both are exactly representable in DECIMAL and NOT in binary —
            // the whole class of price this bug corrupts.
            bid_price: 10.20,
            ask_price: 23925.65,
        }; 5];
        let rows = append_inline_depth(
            &mut sink,
            &tick,
            &levels,
            1_700_000_000_000_000_000,
            0,
            0,
            counters(),
        );
        assert_eq!(rows, 10, "5 levels x 2 sides");

        let ilp = sink.pending_ilp();
        assert!(
            !ilp.contains("10.1999"),
            "the f32 widening artifact for 10.20 reached the wire: {ilp}"
        );
        assert!(
            !ilp.contains("23925.6503"),
            "the f32 widening artifact for 23925.65 reached the wire: {ilp}"
        );
        assert!(
            ilp.contains("price=10.2"),
            "the operator-visible bid price is missing: {ilp}"
        );
        assert!(
            ilp.contains("price=23925.65"),
            "the operator-visible ask price is missing: {ilp}"
        );
    }

    #[test]
    fn append_inline_depth_writes_zero_levels_but_refuses_negative() {
        // Zero is the documented ABSENT-LEVEL sentinel and is written, matching
        // the dedicated drain exactly — if d5 skipped them, the same instrument
        // would report 5 levels in one table and fewer in another for the same
        // instant, and that gap would read as data loss rather than a
        // convention mismatch. A NEGATIVE price is impossible and is refused.
        let mut sink = DepthIngest::for_test();
        let tick = tickvault_common::tick_types::ParsedTick {
            security_id: 13,
            exchange_segment_code: 0,
            ..Default::default()
        };
        let zero = [tickvault_common::tick_types::MarketDepthLevel::default(); 5];
        assert_eq!(
            append_inline_depth(&mut sink, &tick, &zero, 1, 0, 0, counters()),
            10,
            "zero-priced levels are the absent sentinel and MUST be written"
        );

        let mut neg = [tickvault_common::tick_types::MarketDepthLevel::default(); 5];
        neg[0].bid_price = -1.0;
        assert_eq!(
            append_inline_depth(&mut sink, &tick, &neg, 1, 0, 0, counters()),
            9,
            "a negative price is impossible and must be refused"
        );
    }

    #[test]
    fn two_packets_for_one_instrument_in_one_frame_get_distinct_capture_seqs() {
        // THE REGRESSION. A Dhan frame STACKS packets and `received_at_nanos`
        // is computed once for the whole frame, so when the same instrument
        // ticks twice in one frame every DEDUP key column is fixed except
        // `capture_seq`. Until 2026-08-20 that column held `level_no * 2 (+1)`
        // — a function of the level and side ALONE — so the second packet's
        // ten rows carried keys identical to the first packet's and QuestDB
        // upserted one book silently over the other.
        //
        // It could not be caught by a row count: `append_row` succeeds for
        // both packets, so `rows` reads 20 while the table ends up holding 10.
        // The only way to see it is to read the emitted line protocol, which
        // is exactly why `buffer_utf8` was made cross-crate `pub`.
        let mut sink = DepthIngest::for_test();
        let tick = tickvault_common::tick_types::ParsedTick {
            security_id: 13,
            exchange_segment_code: 0,
            ..Default::default()
        };
        let levels = [tickvault_common::tick_types::MarketDepthLevel {
            bid_price: 100.0,
            ask_price: 101.0,
            ..Default::default()
        }; 5];

        // Same frame (seq 4096, base-aligned), packets 0 and 1.
        let frame_seq = 4096_u64;
        let a = append_inline_depth(&mut sink, &tick, &levels, 1, frame_seq, 0, counters());
        let b = append_inline_depth(&mut sink, &tick, &levels, 1, frame_seq, 1, counters());
        assert_eq!(a + b, 20, "both packets appended their ten rows");

        let ilp = sink.writer.buffer_utf8();
        let mut seqs: Vec<&str> = ilp
            .lines()
            .filter_map(|l| l.split("capture_seq=").nth(1))
            .map(|rest| rest.split([',', ' ']).next().unwrap_or(""))
            .collect();
        assert_eq!(seqs.len(), 20, "every row carries a capture_seq");
        seqs.sort_unstable();
        seqs.dedup();
        assert_eq!(
            seqs.len(),
            2,
            "the two PACKETS must differ; the ten rows within a packet share a \
             seq deliberately because `side` and `level` are key columns. Got \
             {seqs:?} — one distinct value means the packet index is not folded \
             in and one book silently overwrites the other"
        );
    }

    #[test]
    fn inline_depth_rows_are_stamped_ist_like_every_sibling_depth_row() {
        // `d5`, `d20` and `d200` land in the SAME table. The dedicated drain
        // adds the IST offset at its stamping site; this path shipped on
        // 2026-08-19 without it, so `d5` rows sat 5h30m behind their siblings
        // — any join or eyeball comparison misaligned silently — and, because
        // `ts` is the DESIGNATED timestamp, every row stamped between 18:30 and
        // 23:59 IST partitioned into the PREVIOUS day, which is the day
        // retention and S3 archival key on.
        let mut sink = DepthIngest::for_test();
        let tick = tickvault_common::tick_types::ParsedTick {
            security_id: 13,
            exchange_segment_code: 0,
            ..Default::default()
        };
        let levels = [tickvault_common::tick_types::MarketDepthLevel {
            bid_price: 100.0,
            ask_price: 101.0,
            ..Default::default()
        }; 5];
        let utc = 1_700_000_000_000_000_000_i64;
        assert_eq!(
            append_inline_depth(&mut sink, &tick, &levels, utc, 0, 0, counters()),
            10
        );
        let expected = utc + tickvault_common::constants::IST_UTC_OFFSET_NANOS;
        let ilp = sink.writer.buffer_utf8();
        let stamped: Vec<&str> = ilp
            .lines()
            .filter_map(|l| l.rsplit(' ').next())
            .filter(|s| !s.is_empty())
            .collect();
        assert!(
            stamped.iter().all(|s| *s == expected.to_string()),
            "every d5 row must carry the IST-shifted stamp {expected}, not raw \
             UTC {utc}. Got {stamped:?}"
        );
    }

    #[test]
    fn append_inline_depth_refuses_an_unknown_segment_rather_than_labelling_it() {
        // Writing an unrecognised segment under a placeholder would silently
        // merge distinct instruments under one label. Refuse, same as the
        // dedicated drain.
        let mut sink = DepthIngest::for_test();
        let tick = tickvault_common::tick_types::ParsedTick {
            security_id: 13,
            exchange_segment_code: 250, // not a real segment
            ..Default::default()
        };
        let levels = [tickvault_common::tick_types::MarketDepthLevel {
            bid_price: 100.0,
            ask_price: 101.0,
            ..Default::default()
        }; 5];
        assert_eq!(
            append_inline_depth(&mut sink, &tick, &levels, 1, 0, 0, counters()),
            0,
            "an unknown segment must produce NO rows"
        );
    }

    /// RATCHET for the 2026-08-25 silent-drop fix in `append_inline_depth`.
    ///
    /// `DEPTH_COUNTER`'s own doc says `refused` covers "parse error, unmappable
    /// segment code, truncated frame tail, or an ILP append failure". The
    /// DEDICATED depth drain honoured that. The INLINE d5 twin, written four
    /// days later, did not: an unmappable segment dropped ten rows, an
    /// out-of-range id dropped ten rows, an implausible price dropped one, and
    /// a failed ILP append dropped one — every one of them with no counter and
    /// no log.
    ///
    /// That is worse than an unmeasured loss. A reader auditing
    /// `tv_dhan_feed_depth_total{outcome="refused"}` would have concluded d5
    /// losses were visible, because the counter's documentation promised
    /// coverage the code never delivered.
    ///
    /// This is a SOURCE SCAN rather than a counter assertion because the
    /// metrics registry is process-global and a delta assertion would be flaky
    /// under the parallel test harness. It fails the build if any of the four
    /// arms loses its counter.
    #[test]
    fn every_inline_depth_drop_is_counted_never_silent() {
        let src = include_str!("dhan_feed_stack.rs");
        let start = src
            .find("fn append_inline_depth(")
            .expect("append_inline_depth must exist"); // APPROVED: test
        let body = &src[start..];
        let end = body
            .find("\nfn drain_depth_frame")
            .expect("the function must be followed by drain_depth_frame"); // APPROVED: test
        let body = &body[..end];

        // 1 + 2: every early `return 0;` must be counted.
        let returns = body.matches("return 0;").count();
        assert!(
            returns >= 3,
            "expected the segment / id / capture_seq refusal arms; found {returns}"
        );
        for (i, chunk) in body.split("return 0;").enumerate() {
            if i >= returns {
                break; // the tail after the last `return 0;`
            }
            assert!(
                chunk.contains("depth_refused.increment(1);"),
                "the early return #{i} in append_inline_depth drops depth rows \
                 with NO counter — that is the silent-drop class this ratchet exists \
                 to forbid"
            );
        }

        // 3 + 4: both `append_row` sites must carry an else arm, and both
        // `plausible` guards must too. Two sides x two guards = four counters
        // beyond the three refusal arms.
        let counted = body.matches("depth_refused.increment(1);").count();
        assert!(
            counted >= 7,
            "append_inline_depth must count every drop: 3 early refusals + 2 \
             implausible-price arms + 2 failed-append arms = 7 minimum; found {counted}"
        );
        assert_eq!(
            body.matches("if sink.writer.append_row(&row).is_ok() {")
                .count(),
            2,
            "both the bid and ask sides append exactly one row"
        );
        assert!(
            !body.contains(
                "rows = rows.saturating_add(1);\n            }\n        }\n        if plausible"
            ),
            "the bid-side append has no else arm — a failed ILP append is being \
             dropped in silence"
        );
    }
    #[test]
    fn flush_reaches_the_depth_sink_even_with_no_pending_tick_rows() {
        // The bug this pins: `flush()` early-returns when `pending_rows == 0`,
        // and my first wiring put the depth flush BELOW that return. Depth and
        // tick rows are appended on different conditions — a Full-mode packet
        // whose tick the aggregator refuses still contributed depth rows — so
        // `pending_rows` can be 0 while the depth buffer is not empty.
        //
        // Appended-but-never-flushed rows do not exist, however green the
        // counters look. This asserts the depth flush runs regardless.
        let mut ingest = LiveIngest::new(TickWriter::for_test(Feed::Dhan), 4)
            .with_inline_depth(DepthIngest::for_test());
        let tick = tickvault_common::tick_types::ParsedTick {
            security_id: 13,
            exchange_segment_code: 0,
            ..Default::default()
        };
        let levels = [tickvault_common::tick_types::MarketDepthLevel {
            bid_price: 100.0,
            ask_price: 101.0,
            ..Default::default()
        }; 5];
        let sink = ingest.inline_depth.as_mut().expect("enabled above");
        let rows = append_inline_depth(sink, &tick, &levels, 1, 0, 0, counters());
        assert_eq!(rows, 10, "depth rows were appended");

        // No ticks were folded, so pending_rows is 0 and the early return
        // fires. The flush must still have reached the depth sink — it
        // returns 0 (no tick rows covered) without panicking or skipping.
        assert_eq!(ingest.flush(), 0, "no tick rows to cover");
        assert!(
            ingest.inline_depth.is_some(),
            "the sink must survive the flush for the next packet"
        );
    }

    /// `LiveIngest::flush` flushes the inline-depth sink EVEN WHEN the tick
    /// writer has been offloaded — which is the fact a source-scan exception
    /// got wrong on 2026-08-25 and left a blocking HTTP call on the drain.
    ///
    /// The exception in `flush_and_record` read the offloaded path as "a
    /// bounded-queue hand-off with no network in it" and returned
    /// `ingest.flush()` bare on that basis. It was reasoning about the TICK
    /// writer, which is true of it, and never re-derived what the callee also
    /// does: the depth sink is flushed first, unconditionally, above the
    /// `pending_rows == 0` early return, and `DepthIngest::flush` is a real
    /// blocking ILP-over-HTTP round trip (`request_timeout=5000`).
    ///
    /// This is written as a BEHAVIOURAL test on purpose. The invariant is also
    /// pinned by a source scan a few hundred lines up, but a source scan
    /// asserts the shape of the code, and the defect here was in a CLAIM about
    /// the code's behaviour — the exact thing a text match cannot check. If
    /// someone ever "optimises" `LiveIngest::flush` by moving the inline-depth
    /// flush below the early return, every source scan still passes and this
    /// test is the only thing that notices.
    ///
    /// `DepthWriter::for_test` has no sender, so its flush discards and errors
    /// — which is precisely the signal we want: `dropped_rows` rising is proof
    /// the flush was ATTEMPTED, and nothing else in this path moves it.
    #[test]
    fn the_offloaded_path_still_flushes_the_inline_depth_sink() {
        let mut sink = DepthIngest::for_test();
        let tick = tickvault_common::tick_types::ParsedTick {
            security_id: 13,
            exchange_segment_code: 0,
            ..Default::default()
        };
        let levels = [tickvault_common::tick_types::MarketDepthLevel {
            bid_quantity: 10,
            ask_quantity: 20,
            bid_orders: 1,
            ask_orders: 2,
            bid_price: 100.5,
            ask_price: 100.75,
        }; 5];
        let appended = append_inline_depth(
            &mut sink,
            &tick,
            &levels,
            1_700_000_000_000_000_000,
            0,
            0,
            counters(),
        );
        assert_eq!(appended, 10, "fixture must actually buffer depth rows");
        assert_eq!(
            sink.dropped_rows(),
            0,
            "nothing may have been discarded before the flush, or the \
             assertion below proves nothing"
        );

        let mut ingest =
            LiveIngest::new(TickWriter::for_test(Feed::Dhan), 8).with_inline_depth(sink);
        let health = Arc::new(tickvault_common::feed_health::FeedHealthRegistry::new());
        ingest
            .spawn_offload_writer(health)
            .expect("offload writer must spawn");
        assert!(
            ingest.writer_is_offloaded(),
            "this test is only meaningful on the OFFLOADED path — the one \
             production takes and the one the withdrawn exception exempted"
        );

        // ZERO tick rows pending, so `LiveIngest::flush` returns 0 by its own
        // early return. The depth sink must still have been flushed.
        assert_eq!(ingest.flush(), 0, "no tick rows to cover");
        let depth = ingest
            .inline_depth
            .as_ref()
            .expect("the sink must survive the flush");
        assert_eq!(
            depth.dropped_rows(),
            10,
            "the inline-depth flush must run on the OFFLOADED path too. Zero \
             here means `LiveIngest::flush` skipped the depth sink, and the \
             `flush_and_record` fast path would then be free of network I/O — \
             which is what the 2026-08-25 exception assumed and what left a \
             5-second blocking HTTP call on the async drain task"
        );
        assert_eq!(
            depth.pending_rows(),
            0,
            "a discarded buffer must not still read as pending"
        );
    }
}

/// Accounting for the two arms where the frame walk gives up part-way.
#[cfg(test)]
mod frame_walk_accounting_tests {
    use super::*;

    /// An arbitrary real epoch second. The session window does not matter
    /// here: an out-of-session tick still WRITES A ROW and still counts as
    /// folded, so these assertions hold whatever the clock says.
    const ANY_LTT: u32 = 1_755_141_600;

    fn ticker_packet(security_id: u32, ltp: f32, ltt: u32) -> [u8; 16] {
        let mut p = [0u8; 16];
        p[0] = 2; // response code: ticker
        p[1] = 16; // message length
        p[3] = 0; // exchange segment: IDX_I
        p[4..8].copy_from_slice(&security_id.to_le_bytes());
        p[8..12].copy_from_slice(&ltp.to_le_bytes());
        p[12..16].copy_from_slice(&ltt.to_le_bytes());
        p
    }
    /// A frame that stacks many packets and hits an unknown response code
    /// part-way must report HOW MUCH it threw away, not just that it gave up.
    ///
    /// The give-up itself is correct and is not what this tests: resynchronising
    /// on a guess would fabricate ticks out of misaligned bytes, which is worse
    /// than losing them. What was wrong was the accounting — both give-up arms
    /// bumped the frame counter by ONE, so a frame that dropped 1,500 packets
    /// and a frame that dropped one reported the same number. An operator
    /// reading `unparseable = 1` would reasonably conclude a single bad packet.
    #[test]
    fn an_unknown_packet_code_reports_the_bytes_it_abandoned() {
        let good = ticker_packet(13, 100.5, ANY_LTT);
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&good);
        bytes.extend_from_slice(&good);
        // An unrecognised response code, followed by what would have been two
        // more perfectly good ticker packets.
        bytes.push(0xFE);
        bytes.extend_from_slice(&[0u8; 15]);
        bytes.extend_from_slice(&good);
        bytes.extend_from_slice(&good);

        let mut ingest = LiveIngest::new(TickWriter::for_test(Feed::Dhan), 8);
        let out = drain_main_feed_frame(
            &mut ingest,
            &CapturedFrame {
                seq: 1,
                endpoint: DhanEndpointType::MainFeed,
                connection_index: 0,
                received_at: std::time::Instant::now(),
                bytes: bytes.into(),
            },
            1_000_000,
            1_000,
            counters(),
        );

        assert_eq!(out.folded, 2, "the two packets before the bad code fold");
        assert_eq!(out.unparseable, 1, "one give-up, as before");
        assert_eq!(
            out.abandoned_bytes, 48,
            "16 bytes of the unknown packet plus the two 16-byte ticker packets \
             behind it were thrown away — reporting 1 here is what made a \
             large loss look like a small one. outcome={out:?}"
        );
    }

    /// Non-vacuity: a frame that decodes cleanly must abandon NOTHING, or the
    /// assertion above would pass against a counter that always fires.
    #[test]
    fn a_clean_frame_abandons_no_bytes() {
        let good = ticker_packet(13, 100.5, ANY_LTT);
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&good);
        bytes.extend_from_slice(&good);

        let mut ingest = LiveIngest::new(TickWriter::for_test(Feed::Dhan), 8);
        let out = drain_main_feed_frame(
            &mut ingest,
            &CapturedFrame {
                seq: 1,
                endpoint: DhanEndpointType::MainFeed,
                connection_index: 0,
                received_at: std::time::Instant::now(),
                bytes: bytes.into(),
            },
            1_000_000,
            1_000,
            counters(),
        );

        assert_eq!(out.folded, 2);
        assert_eq!(out.unparseable, 0);
        assert_eq!(out.abandoned_bytes, 0, "a clean frame loses nothing");
    }

    /// A trailing PARTIAL packet is the other give-up arm and must account the
    /// same way — it was the second half of the same blind spot.
    #[test]
    fn a_truncated_trailing_packet_reports_its_abandoned_bytes() {
        let good = ticker_packet(13, 100.5, ANY_LTT);
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&good);
        // A ticker header promising 16 bytes, with only 9 present.
        bytes.extend_from_slice(&good[..9]);

        let mut ingest = LiveIngest::new(TickWriter::for_test(Feed::Dhan), 8);
        let out = drain_main_feed_frame(
            &mut ingest,
            &CapturedFrame {
                seq: 1,
                endpoint: DhanEndpointType::MainFeed,
                connection_index: 0,
                received_at: std::time::Instant::now(),
                bytes: bytes.into(),
            },
            1_000_000,
            1_000,
            counters(),
        );

        assert_eq!(out.folded, 1);
        assert_eq!(
            out.abandoned_bytes, 9,
            "the 9 bytes of the partial packet are lost and must be counted"
        );
    }
}

#[cfg(test)]
mod late_seed_tests {
    use super::*;

    /// The drain must accept a seed batch and make those instruments visible
    /// to the silence detector.
    ///
    /// This is the whole point: ~20,000 contracts attach after boot, and a
    /// subscribe that silently delivers nothing has NO other evidence — no
    /// payload to count, no parse to fail, no error to log. Absence measured
    /// against a seeded key is the only thing that can ever report it.
    #[test]
    fn seeding_after_boot_makes_an_instrument_visible_to_the_detector() {
        let mut ingest = LiveIngest::new(TickWriter::for_test(Feed::Dhan), 64);
        let before = ingest.tracked_instruments();
        assert!(ingest.seed(48_100, ExchangeSegment::NseFno, 1_000));
        assert_eq!(
            ingest.tracked_instruments(),
            before + 1,
            "a late-attached contract must become tracked, or it can never be \
             reported silent"
        );
    }

    /// Seeding the same instrument twice must not grow the tracked count. The
    /// attach loop can legitimately re-dial after a refusal, and a detector
    /// whose count drifted upward on every retry would make the silence
    /// gauges meaningless.
    ///
    /// Note what `seed` returns: whether the slot allocator ACCEPTED the
    /// instrument, not whether it was new. A repeat seed is a successful
    /// no-op and returns true — which is why the assertion below is on the
    /// COUNT and not on the return value.
    #[test]
    fn re_seeding_the_same_instrument_is_idempotent() {
        let mut ingest = LiveIngest::new(TickWriter::for_test(Feed::Dhan), 64);
        assert!(ingest.seed(48_100, ExchangeSegment::NseFno, 1_000));
        let after_first = ingest.tracked_instruments();
        assert!(
            ingest.seed(48_100, ExchangeSegment::NseFno, 2_000),
            "a repeat seed still succeeds — it is a no-op, not a refusal"
        );
        assert_eq!(
            ingest.tracked_instruments(),
            after_first,
            "but it must not grow the tracked count"
        );
    }
    /// The composite key, not the id alone. Two instruments can share a
    /// numeric id across segments, and collapsing them would leave one of two
    /// real contracts unmonitored while the count looked right.
    #[test]
    fn the_same_id_in_two_segments_seeds_two_instruments() {
        let mut ingest = LiveIngest::new(TickWriter::for_test(Feed::Dhan), 64);
        assert!(ingest.seed(27, ExchangeSegment::IdxI, 1_000));
        assert!(ingest.seed(27, ExchangeSegment::NseEquity, 1_000));
        assert_eq!(ingest.tracked_instruments(), 2);
    }

    /// The drain must carry a seed arm at all, and it must not be biased ahead
    /// of frames. Seeding is bookkeeping; a queued frame is data.
    #[test]
    fn the_drain_has_a_seed_arm_that_does_not_outrank_frames() {
        let src = include_str!("dhan_feed_stack.rs");
        let drain = src
            .split_once("async fn run_frame_drain")
            .expect("the drain must exist")
            .1;
        let seed_arm = drain
            .find("maybe_seed = seed_rx.recv()")
            .expect("the drain must have a seed arm, or late-attached instruments are invisible");
        let frame_arm = drain
            .find("maybe_frame = rx.recv()")
            .expect("the drain must have a frame arm");
        assert!(
            frame_arm < seed_arm,
            "the frame arm must come FIRST under `biased;` — seeding must never \
             preempt draining queued frames"
        );
    }

    #[test]
    fn report_unfolded_wal_frames_is_silent_on_an_empty_batch() {
        // A boot with nothing to recover is the normal case. Reporting there
        // would make the loss line meaningless by printing it every morning.
        report_unfolded_wal_frames(&[], "plan_failed");
    }

    #[test]
    fn report_unfolded_wal_frames_names_the_refusal_it_was_given() {
        // The refusal string is the whole diagnostic value of the line: it is
        // what separates "the pool could not be planned" from "authentication
        // did not finish", which need different responses from the operator.
        let frames = vec![(1u64, bytes::Bytes::from_static(&[8, 0, 0, 0]))];
        report_unfolded_wal_frames(&frames, "token_manager_missing");
        let src = include_str!("dhan_feed_stack.rs");
        let body = src
            .split("pub fn report_unfolded_wal_frames")
            .nth(1)
            .expect("report_unfolded_wal_frames must exist");
        let decl = &body[..body.find("\n}\n").unwrap_or(body.len())];
        assert!(
            decl.contains("refusal,"),
            "the refusal must be a structured field, not only interpolated prose"
        );
        assert!(
            decl.contains("tv_ws_frame_wal_reinjected_dropped_total"),
            "the report must increment the same counter main.rs uses for its own \
             drop path, so the two paths sum into one number instead of two"
        );
    }

    #[test]
    fn every_lane_refusal_before_the_refold_reports_its_unfolded_frames() {
        // THE RATCHET. `main.rs` skips its own drop-and-log whenever
        // `feed_stack_gate` says the lane will run, and `confirm_replayed`
        // archives the segments immediately — before this lane has folded
        // anything. So a refusal that returns before the re-fold without
        // reporting makes captured ticks disappear with no line anywhere.
        //
        // Five such refusals existed and none of them reported. This test
        // fails the build if a sixth is added the same way.
        let src = include_str!("dhan_feed_stack.rs");
        let body = src
            .split("async fn run_dhan_feed_stack")
            .nth(1)
            .expect("run_dhan_feed_stack must exist");
        let upto = body
            .find("let outcome = refold_wal_frames(")
            .expect("the re-fold call must exist — it is what these returns precede");
        let before_refold = &body[..upto];

        let mut unguarded = Vec::new();
        let mut previous = "";
        for (idx, raw) in before_refold.lines().enumerate() {
            let line = raw.trim();
            if line == "return;" && !previous.contains("report_unfolded_wal_frames(") {
                unguarded.push(format!("{}: {previous}", idx + 1));
            }
            if !line.is_empty() {
                previous = line;
            }
        }
        assert!(
            unguarded.is_empty(),
            "every early return before the WAL re-fold must first call \
             report_unfolded_wal_frames — otherwise frames main.rs handed over, and \
             whose segments are already archived, vanish with no log line. Unguarded: {unguarded:?}"
        );

        // Non-vacuous: the scan must actually be looking at real returns.
        assert!(
            before_refold.matches("return;").count() >= 5,
            "the scan window collapsed — it should span every bring-up refusal"
        );
    }

    #[test]
    fn the_fetched_vendor_tape_is_persisted_not_discarded() {
        // OPERATOR INSTRUCTION 2026-08-26. Until today this comparison
        // fetched Dhan's tape, judged it in memory, and threw it away — only
        // the cells that DISAGREED survived. So "what did Dhan say for this
        // instrument at 09:16?" was unanswerable unless that minute happened
        // to diverge, and re-verifying meant ~868 more rate-limited requests.
        //
        // A source scan rather than a runtime assertion because the persist
        // sits behind a live QuestDB connection.
        let src = include_str!("dhan_feed_stack.rs");
        let marker = "fn persist_xverify_report";
        let idx = src.find(marker).expect("the persist fn must exist");
        let body = &src[idx..];
        let end = body.find("\n}\n").unwrap_or(body.len());
        let body = &body[..end];

        assert!(
            body.contains("append_rest_tape"),
            "the fetched vendor tape must be written, or this reverts to \
             fetch-compare-discard and the raw record exists nowhere"
        );
        assert!(
            body.contains("tape_errors"),
            "a partial tape write must be counted — an audit table that \
             silently drops rows is worse than one honestly incomplete"
        );
    }

    #[test]
    fn the_tape_is_stamped_per_target_not_once_per_run() {
        // The fetch loop runs for up to ten minutes. One run-level stamp
        // would claim the last instrument was fetched at the same instant as
        // the first, destroying the only number that says how stale the
        // vendor's own record was when we read it.
        let src = include_str!("dhan_live_crossverify.rs");
        let loop_start = src
            .find("for offset in 0..target_count")
            .expect("the fetch loop must exist");
        let loop_body = &src[loop_start..];
        let stamp = loop_body
            .find("fetched_at_ist_nanos_now")
            .expect("the tape must be stamped INSIDE the fetch loop");
        let push = loop_body
            .find("rest_tape.extend")
            .expect("the tape must be built inside the fetch loop");
        assert!(
            stamp < push,
            "the stamp must be taken before the rows that carry it"
        );
    }
}

#[cfg(test)]
mod depth_rebalance_wiring_tests {
    use super::*;

    fn instrument(id: u64) -> SubscribeInstrument {
        SubscribeInstrument {
            security_id: id,
            segment: tickvault_common::types::ExchangeSegment::NseFno,
        }
    }

    fn dialed(
        endpoint: DhanEndpointType,
        instruments: Vec<SubscribeInstrument>,
    ) -> (
        DhanEndpointType,
        tokio::sync::mpsc::Sender<LiveSubscriptionCommand>,
        Vec<SubscribeInstrument>,
    ) {
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        // Keep the receiver alive for the life of the test so a closed channel
        // never masquerades as a filtered one.
        std::mem::forget(rx);
        (endpoint, tx, instruments)
    }

    #[test]
    fn only_depth_200_connections_are_steerable() {
        // A depth-20 connection holds up to 50 instruments, so a one-for-one
        // swap would drop one of fifty and add another — a valid wire
        // operation and completely the wrong one.
        let got = depth200_rebalance_sockets(vec![
            dialed(DhanEndpointType::Depth200, vec![instrument(1)]),
            dialed(DhanEndpointType::Depth20, vec![instrument(2)]),
            dialed(DhanEndpointType::MainFeed, vec![instrument(3)]),
            dialed(DhanEndpointType::OrderUpdate, vec![]),
            dialed(DhanEndpointType::Depth200, vec![instrument(4)]),
        ]);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].held, Some(instrument(1)));
        assert_eq!(got[1].held, Some(instrument(4)));
    }

    #[test]
    fn dial_order_is_preserved_because_the_planner_indexes_by_it() {
        // plan_swaps assumes NIFTY call, NIFTY put, BANKNIFTY call, BANKNIFTY
        // put in dial order. Reordering here would swap NIFTY's call onto
        // BANKNIFTY's socket — a perfectly valid subscription to entirely the
        // wrong contract.
        let got = depth200_rebalance_sockets(vec![
            dialed(DhanEndpointType::Depth200, vec![instrument(10)]),
            dialed(DhanEndpointType::Depth200, vec![instrument(20)]),
            dialed(DhanEndpointType::Depth200, vec![instrument(30)]),
            dialed(DhanEndpointType::Depth200, vec![instrument(40)]),
        ]);
        let ids: Vec<u64> = got
            .iter()
            .filter_map(|s| s.held.map(|i| i.security_id))
            .collect();
        assert_eq!(ids, vec![10, 20, 30, 40]);
    }

    #[test]
    fn a_connection_dialed_with_more_than_one_instrument_is_skipped_not_truncated() {
        // Taking the first of several would leave the rest untracked AND shift
        // every socket index after it, so the planner would steer the wrong
        // underlying's socket for the whole session.
        let got = depth200_rebalance_sockets(vec![
            dialed(
                DhanEndpointType::Depth200,
                vec![instrument(1), instrument(2)],
            ),
            dialed(DhanEndpointType::Depth200, vec![instrument(3)]),
        ]);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].held, Some(instrument(3)));
    }

    #[test]
    fn a_connection_dialed_with_nothing_is_skipped() {
        let got = depth200_rebalance_sockets(vec![dialed(DhanEndpointType::Depth200, vec![])]);
        assert!(got.is_empty());
    }

    #[test]
    fn no_depth_200_connection_yields_no_sockets() {
        let got = depth200_rebalance_sockets(vec![dialed(
            DhanEndpointType::Depth20,
            vec![instrument(1)],
        )]);
        assert!(got.is_empty());
    }

    #[test]
    fn the_attach_hands_its_channels_over_before_returning() {
        // The whole point of this change. `depth_commands` used to be dropped
        // on return, closing every channel the instant the attach finished, so
        // the entire rebalance was unreachable. A source pin, because the only
        // symptom of losing it is that nothing ever happens.
        let source = include_str!("dhan_feed_stack.rs");
        let production = source
            .split_once("\n#[cfg(test)]")
            .map_or(source, |(before, _)| before);
        assert!(
            production.contains("spawn_depth_rebalance(")
                && production.contains("std::mem::take(&mut depth_commands)"),
            "the attach must MOVE its depth channels into the rebalance; dropping them \
             closes every channel and the machinery goes silently inert"
        );
    }
}

#[cfg(test)]
mod depth20_layout_wiring_tests {
    fn production() -> &'static str {
        let source = include_str!("dhan_feed_stack.rs");
        source
            .split_once("\n#[cfg(test)]")
            .map_or(source, |(before, _)| before)
    }

    #[test]
    fn depth20_tracking_is_wired_into_the_rebalance_spawn() {
        // Without this the depth-20 layout is a boot-time choice that decays
        // all session: NIFTY walks off its own +/-12 window on a 150-point
        // move, and the movers sockets keep a 09:16 ranking.
        let p = production();
        assert!(
            p.contains("depth20_track_sockets(&dialed)"),
            "the spawn no longer builds the depth-20 sockets, so nothing tracks them"
        );
        assert!(
            p.contains("depth20,"),
            "the depth-20 sockets are built and then not handed to the loop"
        );
    }

    #[test]
    fn an_empty_depth200_dial_does_not_take_depth20_tracking_down_with_it() {
        // Two independent pools. The first version of this guard returned on
        // depth-200 alone, which would have killed depth-20 tracking for the
        // whole session on any morning the depth-200 dial came back empty.
        let p = production();
        assert!(
            p.contains("sockets.is_empty() && depth20.is_empty()"),
            "the rebalance spawn gates on one pool, so the other can be silently lost"
        );
    }

    #[test]
    fn the_attach_dials_the_operator_layout_not_the_adaptive_window() {
        // The adaptive window fills all 250 slots with two indices at strikes
        // fifty steps out and carries no stock at all. It is a valid selection
        // and completely the wrong one.
        assert!(
            production().contains("crate::depth20_layout::build_depth20_layout("),
            "the attach must build the operator layout for depth-20"
        );
    }

    #[test]
    fn an_empty_layout_never_wipes_a_working_selection() {
        // Before the chain publishes the layout has no strikes to centre on
        // and returns nothing. Overwriting a working adaptive selection with
        // an empty one trades "the wrong 250" for "no depth at all", which is
        // strictly worse — and it would look like the layout succeeding.
        let p = production();
        let guarded = p.contains("if layout.instrument_count() > 0 {");
        assert!(
            guarded,
            "the layout must be taken only when it produced instruments"
        );
    }

    #[test]
    fn both_halves_read_one_load_so_they_cannot_disagree() {
        // Two loads a few seconds apart fill the movers sockets from one
        // moment's ranking and choose the fifth depth-200 socket from
        // another's.
        let p = production();
        assert!(
            p.contains("crate::depth_rebalance::load_attach_inputs("),
            "the attach must load candidates and movers once for both halves"
        );
        assert_eq!(
            p.matches("crate::depth_rebalance::load_attach_inputs(")
                .count(),
            1,
            "exactly one load — a second call is a second moment"
        );
    }

    #[test]
    fn the_fifth_socket_is_appended_before_planning_not_dialed_after() {
        // plan_pool assigns instruments to connections in order, so the
        // append is what puts the top mover at index 4. Dialing it afterwards
        // needs the pool a second time, which one task cannot hold twice.
        let p = production();
        let append = p
            .find("selection.depth_200.push(fifth);")
            .expect("the fifth socket must be appended");
        let plan = p
            .find("&selection.depth_200,")
            .expect("the plan must read depth_200");
        assert!(
            append < plan,
            "the fifth socket must be appended BEFORE the pool is planned"
        );
    }
}
