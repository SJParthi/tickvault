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

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use secrecy::ExposeSecret;
use tickvault_common::config::QuestDbConfig;
use tickvault_common::constants::{
    DHAN_MAIN_FEED_WS_BASE_URL, DHAN_TWENTY_DEPTH_WS_BASE_URL, DHAN_TWO_HUNDRED_DEPTH_WS_BASE_URL,
    DISCONNECT_PACKET_SIZE, FULL_QUOTE_PACKET_SIZE, MARKET_STATUS_PACKET_SIZE, OI_PACKET_SIZE,
    PREVIOUS_CLOSE_PACKET_SIZE, QUOTE_PACKET_SIZE, SPOT_1M_REST_INDICES,
    TICK_PERSIST_END_SECS_OF_DAY_IST, TICKER_PACKET_SIZE,
};
use tickvault_common::error_code::ErrorCode;
use tickvault_common::feed::Feed;
use tickvault_common::tick_types::ParsedTick;
use tickvault_common::types::ExchangeSegment;
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
    CapturedFrame, ConnectionSupervisor, PoolSupervisor, RingByteBudget, SubscribeGuard,
    SubscribeGuardRefusal, SubscribeInstrument, WalRingSink, run_connection,
};
use tickvault_storage::depth_persistence::{
    DEPTH_KIND_20, DEPTH_KIND_200, DEPTH_SIDE_ASK, DEPTH_SIDE_BID, DepthRow, DepthWriter,
    depth_segment_label,
};
use tickvault_storage::tick_persistence::TickWriter;
use tickvault_storage::ws_frame_spill::{WsFrameSpill, WsType};
use tickvault_trading::candles::{BufferedSeal, ConsumeStats, FeedStrategy, MultiTfAggregator};
use tracing::{debug, error, info, warn};

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

/// Publish the alive-socket count.
fn publish_alive_connections(alive: usize) {
    // `u32::try_from` then `f64::from`: lossless by construction (bounded by
    // the 16-socket lock) and no lossy `as` cast to justify.
    metrics::gauge!(ALIVE_CONNECTIONS_GAUGE)
        .set(f64::from(u32::try_from(alive).unwrap_or(u32::MAX)));
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
        plan_pool(pool, now, endpoint, set, &mut plan)?;
    }
    Ok(plan)
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
    let connections_to_use = usize::from(available).min(set.len()).max(1);
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
    detector: TickGapDetector,
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
    /// Rows appended to the ILP buffer since the last flush. The buffer is a
    /// staging area, NOT storage: without a flush the rows never leave the
    /// process, so this counter is what makes the flush happen at all.
    pending_rows: u64,
}

impl LiveIngest {
    /// Builds the fold, pre-sized for `capacity` instruments so the slot table
    /// and the detector index never realloc mid-session.
    #[must_use]
    pub fn new(writer: TickWriter, capacity: usize) -> Self {
        Self {
            detector: TickGapDetector::with_capacity(capacity, DetectorConfig::default()),
            aggregator: MultiTfAggregator::with_capacity(FeedStrategy::DEFAULT, capacity),
            writer,
            seq_refused: 0,
            refused_price: 0,
            refused_timestamp: 0,
            refused_slot: 0,
            refused_out_of_session: 0,
            seals_emitted: 0,
            seals_dropped: 0,
            pending_rows: 0,
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
    pub fn flush(&mut self) -> u64 {
        if self.pending_rows == 0 {
            return 0;
        }
        let covered = self.pending_rows;
        self.pending_rows = 0;
        match self.writer.flush() {
            Ok(()) => {
                counters().flush_ok.increment(1);
                covered
            }
            Err(err) => {
                counters().flush_failed.increment(1);
                // 2026-08-12 — the second sentence of this message used to
                // read "The raw frames remain in the write-ahead log and are
                // recoverable by replay." That was wrong in a way that
                // mattered: an operator reading it would treat a flush
                // failure as deferred rather than as loss.
                //
                // Replay does not recover these. It runs at BOOT only, and
                // when it runs, `main.rs` STAGE-C.2b logs the replayed
                // live-feed frames and DROPS them — there is no re-fold path
                // (the fold takes a live ring, not a replay batch). So the
                // frames are preserved on disk as bytes and recoverable only
                // by hand; nothing automatic puts these rows in QuestDB.
                //
                // Kept aligned with the STAGE-C.2b wording deliberately: the
                // two messages describe the same loss from the two ends of
                // it, and they must not tell the operator different stories.
                error!(
                    code = ErrorCode::WsGapConnectionState.code_str(),
                    %err,
                    rows = covered,
                    "live tick flush to QuestDB FAILED — the buffered rows were discarded by \
                     the writer contract and are a counted loss: these ticks are NOT in the \
                     database and nothing re-inserts them. The raw frames are preserved in the \
                     write-ahead log and can be recovered manually, but boot replay DROPS \
                     live-feed frames (there is no re-fold path), so do not wait for a restart \
                     to fix this."
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

        // Gap detector observes unconditionally — see the type docs on order.
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
        let sender = tickvault_storage::seal_writer_runner::global_seal_sender();
        let stats: ConsumeStats = self.aggregator.consume_tick(
            Feed::Dhan,
            tick,
            None,
            |feed, security_id, segment_code, tf, state| {
                let Some(tx) = sender else {
                    dropped = dropped.saturating_add(1);
                    return;
                };
                let seal = BufferedSeal::new(security_id, segment_code, tf, state, feed);
                // `try_send`, never `send().await`: this closure runs inside
                // the per-tick fold, and awaiting here would let a slow seal
                // writer stall tick ingestion.
                if tx.try_send(seal).is_err() {
                    dropped = dropped.saturating_add(1);
                } else {
                    emitted = emitted.saturating_add(1);
                }
            },
        );
        self.seals_emitted = self.seals_emitted.saturating_add(emitted);
        self.seals_dropped = self.seals_dropped.saturating_add(dropped);
        if emitted > 0 {
            counters().seals_emitted.increment(emitted);
        }
        if dropped > 0 {
            counters().seals_dropped.increment(dropped);
        }

        // `refused_timestamp` is checked here alongside the other three. It was
        // missing in an earlier draft, so a tick with an implausible exchange
        // timestamp folded into NOTHING and still fell through to the writer,
        // returning `Folded` — a row stamped at a garbage designated timestamp,
        // reported as success.
        // OUT-OF-SESSION IS NOT A REFUSAL OF THE TICK, only of the candle.
        //
        // Checked FIRST and separately: it is the one condition here where the
        // data is fine and only the BUCKET is missing. The tick still gets its
        // row, so the pre-open window is captured instead of discarded. The
        // three below are different in kind — a NaN price, a timestamp outside
        // a 30-year band, or an unidentifiable instrument — and writing any of
        // them would put a corrupt row in `ticks` under a garbage designated
        // timestamp, which is worse than losing it.
        let candle_only_refusal = stats.out_of_session
            && !stats.refused_price
            && !stats.refused_timestamp
            && !stats.slot_exhausted;

        if stats.refused_price
            || (stats.out_of_session && !candle_only_refusal)
            || stats.slot_exhausted
            || stats.refused_timestamp
        {
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
        let sender = tickvault_storage::seal_writer_runner::global_seal_sender();
        let bars = self
            .aggregator
            .force_seal_all(|feed, security_id, segment_code, tf, state| {
                let Some(tx) = sender else {
                    dropped = dropped.saturating_add(1);
                    return;
                };
                let seal = BufferedSeal::new(security_id, segment_code, tf, state, feed);
                // `try_send` for the same reason the per-tick path uses it: a
                // slow writer must never block the caller. At close the caller
                // is the drain's own shutdown, and blocking it would hold the
                // whole lane open past the session.
                if tx.try_send(seal).is_err() {
                    dropped = dropped.saturating_add(1);
                } else {
                    emitted = emitted.saturating_add(1);
                }
            });
        debug_assert_eq!(
            bars as u64,
            emitted.saturating_add(dropped),
            "every bar force_seal_all produced must be accounted as emitted or dropped"
        );
        self.seals_emitted = self.seals_emitted.saturating_add(emitted);
        self.seals_dropped = self.seals_dropped.saturating_add(dropped);
        if emitted > 0 {
            counters().seals_emitted.increment(emitted);
        }
        if dropped > 0 {
            counters().seals_dropped.increment(dropped);
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
        let mut silent = 0u64;
        let mut never = 0u64;
        // Worst offender, kept for the log line so the operator gets a name
        // and not just a count. One `Copy` key, no allocation.
        let mut worst: Option<(
            u64,
            tickvault_core::pipeline::tick_gap_detector::SilenceReport,
        )> = None;
        self.detector.scan_silence(now_millis, |report| {
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
            if worst.is_none_or(|(w, _)| report.silent_millis > w) {
                worst = Some((report.silent_millis, report));
            }
        });
        metrics::gauge!(INSTRUMENTS_SILENT_GAUGE).set(silent as f64);
        metrics::gauge!(INSTRUMENTS_NEVER_TICKED_GAUGE).set(never as f64);
        if let Some((_, w)) = worst {
            debug!(
                security_id = w.key.0,
                segment = w.key.1.as_str(),
                silent_millis = w.silent_millis,
                expected_millis = w.expected_millis,
                baseline_millis = w.baseline_millis,
                samples = w.samples,
                "quietest tracked instrument this scan"
            );
        }
        (silent, never)
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
    /// watermark itself. The watermark is the highest exchange timestamp seen
    /// across ALL instruments, and ticks arrive out of order between them, so
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
        let sender = tickvault_storage::seal_writer_runner::global_seal_sender();
        let bars = self.aggregator.catch_up_seal_all(
            cutoff,
            |feed, security_id, segment_code, tf, state| {
                let Some(tx) = sender else {
                    dropped = dropped.saturating_add(1);
                    return;
                };
                let seal = BufferedSeal::new(security_id, segment_code, tf, state, feed);
                if tx.try_send(seal).is_err() {
                    dropped = dropped.saturating_add(1);
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
        if emitted > 0 {
            counters().seals_emitted.increment(emitted);
        }
        if dropped > 0 {
            counters().seals_dropped.increment(dropped);
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

    /// Sealed candles handed to the process-wide seal writer.
    #[must_use]
    pub const fn seals_emitted(&self) -> u64 {
        self.seals_emitted
    }

    /// Sealed candles LOST — no seal writer installed, or its queue was full.
    /// Non-zero means candles were computed and discarded, which is the one
    /// number that separates "the lane is producing candles" from "the lane is
    /// burning CPU".
    #[must_use]
    pub const fn seals_dropped(&self) -> u64 {
        self.seals_dropped
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
struct DrainCounters {
    folded: metrics::Counter,
    non_tick: metrics::Counter,
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
    flush_ok: metrics::Counter,
    flush_failed: metrics::Counter,
    depth_unconsumed: metrics::Counter,
    depth_rows: metrics::Counter,
    depth_refused: metrics::Counter,
    depth_dropped: metrics::Counter,
    depth_disconnects: metrics::Counter,
    depth_length_mismatch: metrics::Counter,
    truncated: metrics::Counter,
    xverify_ran: metrics::Counter,
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
fn counters() -> &'static DrainCounters {
    static COUNTERS: std::sync::OnceLock<DrainCounters> = std::sync::OnceLock::new();
    COUNTERS.get_or_init(|| DrainCounters {
        folded: metrics::counter!(DRAIN_FRAMES_COUNTER, "outcome" => "folded"),
        non_tick: metrics::counter!(DRAIN_FRAMES_COUNTER, "outcome" => "non_tick"),
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
        flush_ok: metrics::counter!(FLUSH_COUNTER, "outcome" => "ok"),
        flush_failed: metrics::counter!(FLUSH_COUNTER, "outcome" => "failed"),
        depth_unconsumed: metrics::counter!(DRAIN_FRAMES_COUNTER, "outcome" => "depth_unconsumed"),
        depth_rows: metrics::counter!(DEPTH_COUNTER, "outcome" => "rows"),
        depth_refused: metrics::counter!(DEPTH_COUNTER, "outcome" => "refused"),
        depth_dropped: metrics::counter!(DEPTH_COUNTER, "outcome" => "dropped"),
        depth_disconnects: metrics::counter!(DEPTH_COUNTER, "outcome" => "disconnects"),
        depth_length_mismatch: metrics::counter!(DEPTH_COUNTER, "outcome" => "length_mismatch"),
        truncated: metrics::counter!(DRAIN_FRAMES_COUNTER, "outcome" => "truncated"),
        xverify_ran: metrics::counter!(XVERIFY_RUNS_COUNTER, "outcome" => "ran"),
        xverify_failed: metrics::counter!(XVERIFY_RUNS_COUNTER, "outcome" => "failed"),
        xverify_no_token: metrics::counter!(XVERIFY_RUNS_COUNTER, "outcome" => "no_token"),
    })
}

/// Counter: daily cross-verification attempts, by outcome. Anything other than
/// `ran` means the session's captured candles were never checked against
/// Dhan's own record.
pub const XVERIFY_RUNS_COUNTER: &str = "tv_dhan_feed_xverify_runs_total";

/// Counter: sealed candles handed to the process-wide seal writer.
pub const SEALS_EMITTED_COUNTER: &str = "tv_dhan_feed_seals_emitted_total";

/// Counter: sealed candles LOST — no seal writer installed, or its queue was
/// full. Non-zero means candles were computed and discarded.
pub const SEALS_DROPPED_COUNTER: &str = "tv_dhan_feed_seals_dropped_total";

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
/// frame behind them is then refused. Depth frames are `depth_unconsumed`:
/// counted and DISCARDED, because nothing folds them today. So the shared
/// budget let a stream we throw away evict the only stream we keep.
///
/// Splitting the same total keeps the memory ceiling identical (the host is no
/// worse off) and makes that eviction impossible: depth can exhaust depth.
pub const MAIN_FEED_RING_MAX_BYTES: usize = FRAME_RING_MAX_BYTES * 3 / 4;

/// Depth's share of the same ceiling — the other quarter, for both depth-20 and
/// depth-200 together.
///
/// The split is 3:1 toward the main feed rather than by socket count (5 vs 10)
/// because the split follows the DATA, not the sockets: the main feed carries
/// every tick that reaches the database, and depth currently carries none.
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
) -> DrainOutcome {
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
                let received_at_nanos = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);
                // The gap detector's clock is a millisecond reading; the same
                // wall-clock instant is used so a frame's arrival and its
                // silence-accounting can never disagree.
                let recv_millis = u64::try_from(received_at_nanos / 1_000_000).unwrap_or(0);

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
                            Some(depth) => {
                                let outcome = drain_depth_frame(
                                    depth, &frame, received_at_nanos, kind, c,
                                );
                                depth_rows = depth_rows.saturating_add(outcome.rows);
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
                    blocking_flush(|| ingest.flush());
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
                blocking_flush(|| ingest.flush());
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
                blocking_flush(|| ingest.flush());
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

                let (silent, never) = ingest.scan_silence(now_millis);
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
                    silence_reported = false;
                    continue;
                }
                silent_scans = silent_scans.saturating_add(1);
                if silent_scans >= SILENCE_SCANS_BEFORE_ALERT && !silence_reported {
                    silence_reported = true;
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
    let tail = blocking_flush(|| ingest.flush());
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
fn drain_main_feed_frame(
    ingest: &mut LiveIngest,
    frame: &CapturedFrame,
    received_at_nanos: i64,
    recv_millis: u64,
    c: &DrainCounters,
) -> FrameOutcome {
    let mut out = FrameOutcome::default();
    // A single WebSocket message may carry SEVERAL stacked packets — the
    // frame cap is sized for ~1,600 of them. Walking the frame packet by
    // packet is what stops packets 2..N being silently discarded.
    let mut offset = 0usize;
    let mut packets = 0u32;
    while offset < frame.bytes.len() {
        let Some(len) = main_feed_packet_len(&frame.bytes[offset..]) else {
            // Unrecognised code or a trailing partial packet: stop here rather
            // than resynchronising on a guess, which would fabricate ticks.
            c.unparseable.increment(1);
            out.unparseable = out.unparseable.saturating_add(1);
            return out;
        };
        let end = offset.saturating_add(len);
        if end > frame.bytes.len() {
            c.truncated.increment(1);
            return out;
        }
        match dispatch_frame(&frame.bytes[offset..end], received_at_nanos) {
            Ok(ParsedFrame::Tick(tick) | ParsedFrame::TickWithDepth(tick, _)) => {
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
    /// Packets refused by the parser or by an unknown response code.
    pub unparseable: u64,
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
    // The frame's own replay-stable sequence, reused as `capture_seq`. Minting
    // a fresh one here would make a WAL-replayed depth frame land as a second
    // set of rows instead of collapsing onto the originals — the same trap the
    // tick path documents at `append_tick_with_seq`.
    let capture_seq = i64::try_from(frame.seq).unwrap_or(i64::MAX);

    let mut iter = split_depth_frame(&frame.bytes, kind);
    // `by_ref` rather than consuming the iterator: `stop_reason()` and
    // `length_field_mismatches()` are read AFTER the walk, and they are the
    // only way to tell a cleanly-consumed frame from a truncated one.
    for packet_bytes in iter.by_ref() {
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
                ts_nanos: received_at_nanos,
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

/// Packets we will walk within one main-feed message before declaring the
/// message malformed.
///
/// **The arithmetic here was wrong until 2026-08-14** and is worth stating
/// rather than quietly fixing. The comment claimed "the 1 MiB frame cap over
/// the smallest (16-byte) packet bounds a legitimate message well under this".
/// Two errors: the cap is `MAIN_FEED_MAX_FRAME_BYTES` = 162 × 5,000 × 2 =
/// 1,620,000 bytes (~1.55 MiB, not 1 MiB), and 1,620,000 / 16 = **101,250**,
/// which is ABOVE this ceiling, not well under it. A maximum-size frame made
/// entirely of 16-byte ticker packets would be truncated here, its remainder
/// counted as unparseable.
///
/// The ceiling is nonetheless kept, because it is a defence against a hostile
/// or malfunctioning peer rather than a capacity limit, and the shape it
/// bounds cannot occur legitimately: a socket carries at most
/// `MAIN_FEED_INSTRUMENTS_PER_CONNECTION` (5,000) subscriptions, so a
/// legitimate frame carries on the order of 5,000 packets — 14× below this —
/// and reaching 101,250 would require the peer to send ~20 packets per
/// subscribed instrument in a single message. Raising the ceiling to clear the
/// theoretical maximum would weaken the defence to buy nothing.
///
/// What changed is only the honesty of the justification: the bound is a
/// deliberate policy ceiling, not the arithmetic consequence the old comment
/// asserted.
pub const MAX_PACKETS_PER_FRAME: u32 = 70_000;

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

static WS_LAG_HANDLES: OnceLock<WsLagHandles> = OnceLock::new();

fn ws_lag_handles() -> &'static WsLagHandles {
    WS_LAG_HANDLES.get_or_init(WsLagHandles::new)
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

/// Byte length of the main-feed packet starting at `bytes`, from its response
/// code. `None` for an unknown code or a header too short to classify.
///
/// The header carries its own message length at bytes 1..3, but that field is
/// vendor-supplied: trusting it would let a malformed length walk the parser
/// off the end of one packet and into the middle of the next. The code→size
/// table is ours and is fixed by the protocol.
fn main_feed_packet_len(bytes: &[u8]) -> Option<usize> {
    let code = *bytes.first()?;
    let size = match code {
        // Ticker (2), previous close (6), OI (5), disconnect (50), market
        // status (7) — sizes from `crates/common/src/constants.rs`.
        2 => TICKER_PACKET_SIZE,
        4 => QUOTE_PACKET_SIZE,
        5 => OI_PACKET_SIZE,
        6 => PREVIOUS_CLOSE_PACKET_SIZE,
        7 => MARKET_STATUS_PACKET_SIZE,
        8 => FULL_QUOTE_PACKET_SIZE,
        50 => DISCONNECT_PACKET_SIZE,
        _ => return None,
    };
    Some(size)
}

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

/// IST second-of-day past which the depth late-attach gives up for the session.
///
/// 10:00 IST — 44 minutes after the option-chain leg's first fire. Past this,
/// a still-empty chain is not "not yet", it is broken, and quietly polling a
/// broken table until 15:30 would report nothing while looking busy.
pub const DEPTH_ATTACH_DEADLINE_IST_SECS: u32 = 10 * 3_600;

/// Current IST second-of-day.
fn ist_second_of_day_now() -> u32 {
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
    // Passed only because `dial_planned_connections` selects per endpoint; this
    // path never produces a main-feed connection, and the plan builder is what
    // guarantees that rather than this argument.
    main_feed_budget: Arc<RingByteBudget>,
) {
    let mut attempts: u32 = 0;
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
        if attempts > 0 && ist_second_of_day_now() >= DEPTH_ATTACH_DEADLINE_IST_SECS {
            error!(
                code = ErrorCode::WsGapSubscriptionBatching.code_str(),
                attempts,
                "depth late-attach gave up: option_chain_1m still yielded no depth candidates by \
                 the 10:00 IST deadline, so depth-20 and depth-200 will carry NO data this \
                 session. Check that the option-chain leg is running and that \
                 contract_security_id is populated."
            );
            return;
        }
        // Re-derived every attempt, never hoisted: this task can outlive an
        // IST midnight, and a hoisted date would then query yesterday forever.
        let selection = crate::dhan_depth_universe::load_depth_universe(
            &questdb,
            crate::dhan_universe::ist_midnight_nanos(&crate::dhan_universe::today_ist_date()),
        )
        .await;
        attempts = attempts.saturating_add(1);

        if !selection.depth_20.is_empty() || !selection.depth_200.is_empty() {
            // Upgrade LAST, immediately before dialing. `None` means every
            // socket died and the ring closed while we waited — dialing into a
            // closed ring would open sockets whose frames reach nothing.
            let Some(frame_tx) = frame_weak.upgrade() else {
                warn!(
                    code = ErrorCode::WsGapConnectionState.code_str(),
                    attempts,
                    depth_20 = selection.depth_20.len(),
                    depth_200 = selection.depth_200.len(),
                    "depth late-attach resolved its instruments but the frame ring had already \
                     closed — the lane went dark while it waited. Refusing to dial sockets whose \
                     frames would reach no consumer."
                );
                return;
            };
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
                        &mut pool,
                        &client_id,
                        &spill,
                        &frame_tx,
                        &main_feed_budget,
                        &depth_budget,
                    );
                    info!(
                        dialed,
                        attempts,
                        depth_20 = selection.depth_20.len(),
                        depth_200 = selection.depth_200.len(),
                        "depth late-attach opened its sockets: the option-chain leg published \
                         today's contracts and depth dialed against them without a restart"
                    );
                }
                Err(err) => {
                    error!(
                        code = ErrorCode::WsGapSubscriptionBatching.code_str(),
                        ?err,
                        depth_20 = selection.depth_20.len(),
                        depth_200 = selection.depth_200.len(),
                        "depth late-attach resolved its instruments but planning refused them — \
                         depth carries no data this session"
                    );
                }
            }
            return;
        }
        tokio::time::sleep(std::time::Duration::from_secs(DEPTH_ATTACH_RETRY_SECS)).await;
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
fn dial_planned_connections(
    plan: FeedStackPlan,
    pool: &mut PoolSupervisor,
    client_id: &str,
    spill: &Arc<WsFrameSpill>,
    frame_tx: &tokio::sync::mpsc::Sender<CapturedFrame>,
    main_feed_budget: &Arc<RingByteBudget>,
    depth_budget: &Arc<RingByteBudget>,
) -> usize {
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
        let sink = Arc::new(WalRingSink::new(
            Arc::clone(spill),
            frame_tx.clone(),
            Arc::clone(budget),
            WsType::LiveFeed,
            endpoint,
            // `global_index` (0..16), not `pool_index` — pool indices repeat
            // across endpoints, so labelling by them would merge main-feed
            // socket 0 with depth-20 socket 0 and hide which one is sick.
            planned.slot.global_index,
        ));
        let guard = planned.guard;
        // Count it alive BEFORE the task starts, so the gauge can never read
        // high because a spawn lost a race with its own decrement.
        publish_alive_connections(ALIVE_CONNECTIONS.fetch_add(1, Ordering::SeqCst) + 1);
        tokio::spawn(async move {
            let exit = run_connection(socket, supervisor, guard, sink, || async {
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
            })
            .await;
            // `run_connection` returning means this socket is GONE — parked,
            // errored, or shut down. Nothing re-dials it, and its shard of the
            // universe stops delivering. Publishing here is what turns
            // "sockets we planned" into "sockets that exist".
            let remaining = ALIVE_CONNECTIONS
                .fetch_sub(1, Ordering::SeqCst)
                .saturating_sub(1);
            publish_alive_connections(remaining);
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
                    // because the tick falls outside the aggregating session
                    // (the 09:00–09:15 pre-open). Counting it as `lost` would
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
        return None;
    }
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
        return;
    };

    // ---- the fold ----------------------------------------------------------
    let capacity = params.main_feed_instruments.len()
        + params.depth_20_instruments.len()
        + params.depth_200_instruments.len();
    let mut ingest = LiveIngest::new(
        TickWriter::new(&params.questdb, Feed::Dhan),
        capacity.max(1),
    );

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
    }

    let (frame_tx, frame_rx) = tokio::sync::mpsc::channel::<CapturedFrame>(FRAME_RING_CAPACITY);
    // The ring's second bound, split by ENDPOINT rather than shared.
    //
    // One budget per POOL would bound five times the host's memory, which is
    // why the original was shared. But one budget for ALL endpoints let the
    // depth stream — whose frames are 512 KiB — exhaust the whole ceiling and
    // evict main-feed frames, i.e. every tick that reaches the database. Two
    // budgets summing to the SAME total keep the host ceiling identical while
    // making that eviction impossible.
    //
    // (The original comment said depth's payload was "discarded unparsed".
    // That stopped being true on 2026-08-15, when depth became a persisted
    // stream. The split matters MORE now, not less: depth is no longer merely
    // occupying the ring, it is producing 20–200 database rows per packet, so
    // an unbounded depth burst would compete with ticks for the ILP path too.)
    let main_feed_budget = Arc::new(RingByteBudget::new(MAIN_FEED_RING_MAX_BYTES));
    let depth_budget = Arc::new(RingByteBudget::new(DEPTH_RING_MAX_BYTES));
    // Wired whenever ANY depth socket will be dialed. Built here rather than
    // unconditionally so the drain's `depth_unconsumed` counter keeps its new
    // meaning: with no depth instruments there is no writer, no table traffic,
    // and no depth frames either — the counter stays at zero for the honest
    // reason rather than because nothing was checked.
    let depth_ingest =
        if params.depth_20_instruments.is_empty() && params.depth_200_instruments.is_empty() {
            None
        } else {
            Some(DepthIngest::new(&params.questdb))
        };
    let drain = tokio::spawn(run_frame_drain(
        frame_rx,
        ingest,
        depth_ingest,
        Arc::clone(&main_feed_budget),
        Arc::clone(&depth_budget),
        Arc::clone(&params.shutdown),
    ));

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
    let dialed = dial_planned_connections(
        plan,
        &mut pool,
        &client_id,
        &spill,
        &frame_tx,
        &main_feed_budget,
        &depth_budget,
    );

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

    // Up means SOCKETS DIALED AND A FOLD CONSUMING THEM — not "config was
    // enabled". It is set here and cleared by the drain when the ring closes.
    metrics::gauge!(FEED_STACK_UP_GAUGE).set(1.0);

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

/// Builds the comparator's target list from the subscribed main-feed set, so
/// the lane can never verify a different universe than it captured.
#[must_use]
pub fn crossverify_targets(
    main_feed: &[SubscribeInstrument],
) -> Vec<crate::dhan_live_crossverify::XverifyTarget> {
    main_feed
        .iter()
        .map(|i| crate::dhan_live_crossverify::XverifyTarget {
            security_id: i64::try_from(i.security_id).unwrap_or(0),
            segment: i.segment.as_str().to_string(),
            instrument: "INDEX".to_string(),
        })
        .collect()
}

/// Spawns the daily 15:31 IST comparator for the subscribed universe.
///
/// Returns `None` — loudly — when no [`CrossverifyDeps`] were installed. That
/// is a refusal, not a skip: a live lane with no verifier has no way to detect
/// the packet loss its protocol cannot report, and saying so is the whole
/// point of audit Rule 11.
pub fn spawn_daily_crossverify(
    main_feed: &[SubscribeInstrument],
) -> Option<tokio::task::JoinHandle<()>> {
    let targets = crossverify_targets(main_feed);
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
            run_at_ist = "15:31",
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
                    counters().xverify_ran.increment(1);
                    info!(
                        targets = targets.len(),
                        ?report,
                        "Dhan live-feed cross-verification finished — this is the honest \
                         measure of whether the revived feed agrees with Dhan's own record"
                    );
                }
                Err(err) => {
                    counters().xverify_failed.increment(1);
                    error!(
                        code = ErrorCode::WsGapConnectionState.code_str(),
                        %err,
                        "Dhan live-feed cross-verification FAILED to run — the day's captured \
                         candles are UNVERIFIED, never assume they are clean"
                    );
                }
            }
        }
    }))
}

/// IST seconds-of-day at which the comparator runs: 15:31, one minute after
/// the 15:30 close, so the final minute's candle has sealed.
pub const XVERIFY_RUN_AT_SECS_OF_DAY_IST: u64 = 15 * 3_600 + 31 * 60;

/// Seconds in a day.
const SECS_PER_DAY: u64 = 24 * 3_600;

/// Seconds to sleep from `now_secs_of_day` until the next 15:31 IST.
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

    // -- pool spreading (operator directive 2026-08-12: use all 16) ---------

    /// A set that FITS one connection must still spread across the authorized
    /// five.
    ///
    /// This is the whole point of the change. 4,565 main-feed instruments fit
    /// one 5,000-instrument socket, so packing opened exactly ONE connection
    /// and left four authorized ones idle — which is how "16 authorized" was
    /// really 7 in practice.
    #[test]
    fn test_main_feed_spreads_a_fitting_set_across_all_five_connections() {
        let mut pool = PoolSupervisor::new();
        let now = std::time::Instant::now();
        // The real resolved-master size on the live box, 2026-08-12.
        let plan = build_feed_stack_plan(&mut pool, now, &instruments(4565), &[], &[])
            .expect("4565 instruments must plan cleanly");
        assert_eq!(
            plan.count_for(DhanEndpointType::MainFeed),
            5,
            "4,565 instruments must SPREAD across all 5 authorized main-feed \
             connections; packing put them on 1 and left 4 idle"
        );
    }

    /// The Dhan per-connection cap is absolute and spreading must never breach it.
    ///
    /// Spreading widens shards only when a set fits in FEWER connections than
    /// are authorized, so the width can only shrink relative to the cap — but
    /// this asserts the invariant directly rather than trusting that argument.
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
    fn test_feed_stack_gate_is_shut_by_the_shipped_config() {
        // `dhan_enabled = false` in BOTH config/base.toml and
        // config/production.toml since the 2026-07-13 retirement.
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
    fn test_build_feed_stack_plan_spreads_the_index_universe_one_per_connection() {
        // CHANGED 2026-08-12 (spread, not pack): four index SIDs now open FOUR
        // main-feed sockets, one instrument each, where packing opened one.
        // Bounded by the instrument count, so no socket is empty — four
        // instruments can never justify a fifth connection.
        let mut pool = PoolSupervisor::new();
        let plan = build_feed_stack_plan(
            &mut pool,
            Instant::now(),
            &hardcoded_index_universe(),
            &[],
            &[],
        )
        .expect("four indices plan cleanly");
        assert_eq!(plan.len(), 4);
        assert_eq!(plan.count_for(DhanEndpointType::MainFeed), 4);
        assert_eq!(plan.count_for(DhanEndpointType::Depth20), 0);
        assert_eq!(plan.count_for(DhanEndpointType::Depth200), 0);
        assert_eq!(pool.total_open(), 4);
    }

    #[test]
    fn test_plan_count_for_spreads_across_the_authorized_connections() {
        let mut pool = PoolSupervisor::new();
        // CHANGED 2026-08-12 (spread, not pack). Every set below fits inside
        // the authorized pools, so each SPREADS across all 5 connections
        // rather than packing into the fewest:
        //   12,001 main-feed -> 5 conns of 2,401 (cap 5,000, well inside)
        //      101 depth-20   -> 5 conns of 21    (cap 50)
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
        assert_eq!(plan.count_for(DhanEndpointType::MainFeed), 5);
        assert_eq!(plan.count_for(DhanEndpointType::Depth20), 5);
        assert_eq!(plan.count_for(DhanEndpointType::Depth200), 3);
        assert_eq!(plan.len(), 13);
        assert_eq!(pool.total_open(), 13);
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
        // CHANGED 2026-08-12: spread gives 5 conns of 1,556 rather than 2 of
        // 5,000. Conservation above is the invariant that actually matters and
        // is unchanged.
        assert_eq!(plan.count_for(DhanEndpointType::MainFeed), 5);
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

    fn depth_frame(bytes: Vec<u8>, endpoint: DhanEndpointType, seq: u64) -> CapturedFrame {
        CapturedFrame {
            seq,
            endpoint,
            connection_index: 5,
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
        // Minting a fresh capture_seq here would make a WAL-replayed depth
        // frame land as a SECOND set of rows instead of collapsing onto the
        // originals. Asserted through the outcome rather than the row, because
        // the row is private to the writer's buffer.
        let mut depth = DepthIngest::for_test();
        let frame = depth_frame(depth20_packet(13, 0, 41), DhanEndpointType::Depth20, 4_242);
        let out = drain_depth_frame(
            &mut depth,
            &frame,
            1_779_355_000_000_000_000,
            DepthFeedKind::Twenty,
            counters(),
        );
        assert_eq!(out.rows, 20);
        // Re-folding the SAME frame produces the same 20 rows again — the
        // database collapses them because every key column, capture_seq
        // included, is identical. What must NOT happen is the sequence
        // changing between the two folds, which is what this asserts by
        // construction: `drain_depth_frame` reads `frame.seq` and nothing else.
        let again = drain_depth_frame(
            &mut depth,
            &frame,
            1_779_355_000_000_000_000,
            DepthFeedKind::Twenty,
            counters(),
        );
        assert_eq!(again.rows, 20);
    }

    #[test]
    fn a_depth_frame_with_no_ingest_wired_counts_as_unconsumed_not_as_success() {
        // The `None` arm is a WIRING BUG, not a design choice, and it must be
        // visible as one. This asserts the outcome field exists and is used.
        let out = DepthFrameOutcome::default();
        assert_eq!(out.rows, 0);
        assert_eq!(out.refused, 0);
        assert_eq!(out.disconnects, 0);
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
        // every bar lands on the `dropped` side. That is deliberate and it is
        // still the assertion that matters — a non-zero total proves the
        // aggregator was actually walked and OPEN buckets were found, which
        // is precisely what the missing call site was failing to do. The
        // emitted-vs-dropped SPLIT is exercised by the writer-side tests; the
        // invariant here is that no bar escapes accounting on either side.
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
        let targets = crossverify_targets(&universe);

        assert_eq!(
            targets.len(),
            universe.len(),
            "one target per subscribed instrument, no more and no fewer"
        );
        for (t, i) in targets.iter().zip(universe.iter()) {
            assert_eq!(t.security_id, i64::try_from(i.security_id).expect("fits"));
            assert_eq!(t.segment, i.segment.as_str());
        }
        assert!(
            crossverify_targets(&[]).is_empty(),
            "an empty universe yields no targets rather than a default one"
        );
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
        // 15:31 = one minute after the close, so the final minute has sealed.
        assert_eq!(XVERIFY_RUN_AT_SECS_OF_DAY_IST, 55_860);

        // Before the run time: wait until today's.
        assert_eq!(secs_until_next_run_ist(0), 55_860, "midnight → today 15:31");
        assert_eq!(
            secs_until_next_run_ist(55_859),
            1,
            "one second before → one second to wait"
        );

        // AT the run time: a full day, never zero. Zero would busy-loop the
        // task and fire the comparator repeatedly within one session.
        assert_eq!(
            secs_until_next_run_ist(55_860),
            SECS_PER_DAY,
            "exactly at the run time must wait a full day, not fire again"
        );

        // After: tomorrow's.
        assert_eq!(secs_until_next_run_ist(55_861), SECS_PER_DAY - 1);
        assert_eq!(
            secs_until_next_run_ist(SECS_PER_DAY - 1),
            55_861,
            "one second before midnight → tomorrow 15:31"
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
        assert_eq!(main_feed_packet_len(&[2]), Some(TICKER_PACKET_SIZE));
        assert_eq!(main_feed_packet_len(&[4]), Some(QUOTE_PACKET_SIZE));
        assert_eq!(main_feed_packet_len(&[5]), Some(OI_PACKET_SIZE));
        assert_eq!(main_feed_packet_len(&[6]), Some(PREVIOUS_CLOSE_PACKET_SIZE));
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
        let production_half = src.split(test_marker).next().unwrap_or(src);

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

        // The load-bearing assertion: no production call site may invoke
        // flush() bare. Written as a search for the bare form so that adding a
        // FOURTH flush site without the helper fails here rather than in prod.
        let bare = production_half.matches("ingest.flush()").count();
        let wrapped = production_half
            .matches("blocking_flush(|| ingest.flush())")
            .count();
        assert_eq!(
            bare, wrapped,
            "every production ingest.flush() must be wrapped in blocking_flush; \
             found {bare} call(s) and {wrapped} wrapped — the difference is a \
             blocking HTTP call sitting on the async drain task"
        );
        assert!(
            wrapped >= 3,
            "expected at least the 3 known flush sites (size trigger, time \
             trigger, shutdown tail); found {wrapped} — did a site get deleted?"
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
    fn test_up_gauge_is_raised_only_after_sockets_are_dialed_and_cleared_when_they_die() {
        // Rule 11, no false-OK. Until this round the guard read "nothing may
        // set this gauge to 1" — because no transport existed and reporting
        // the lane up would have been a lie. The transport now exists, so the
        // invariant tightens rather than relaxes: `1` may only be written
        // BELOW the dial loop (never on the config-gate path), and something
        // must write `0` back when the ring closes, or a dead lane would keep
        // reporting itself healthy forever.
        let src = include_str!("dhan_feed_stack.rs");
        let test_marker = concat!("#[cfg(", "test)]");
        let production_half = src.split(test_marker).next().unwrap_or(src);

        let raise = production_half
            .find("gauge!(FEED_STACK_UP_GAUGE).set(1.0)")
            .expect("the lane must report itself up once it is actually carrying data");
        assert_eq!(
            production_half
                .matches("FEED_STACK_UP_GAUGE).set(1.0)")
                .count(),
            1,
            "exactly ONE site may raise the up-gauge, so there is one place to audit"
        );

        // The dial loop's own marker. `1` must come after it — a raise above
        // this line would be reporting config, not connectivity.
        let dialed = production_half
            .find("dialed = dialed.saturating_add(1)")
            .expect("the dial loop must count the sockets it opened");
        assert!(
            raise > dialed,
            "the up-gauge may only be raised after sockets have actually been dialed"
        );

        // And something must clear it. Without this, a lane whose every socket
        // died would sit at 1 until the process restarted. `find`, not
        // `rfind`: there are legitimately TWO clear sites — the drain's normal
        // exit (above the raise in source order) and the panic handler that
        // catches a dead drain (below it). Asserting on the LAST one would
        // demand the impossible, which is how this guard first went red.
        let clear = production_half
            .find("gauge!(FEED_STACK_UP_GAUGE).set(0.0)")
            .expect("something must clear the up-gauge when the lane goes down");
        assert!(
            clear < raise,
            "the drain's clear belongs above bring-up in source order"
        );
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
        use crate::dhan_live_crossverify::{is_in_session, is_tail_minute};

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
        assert_eq!(
            in_session, 375,
            "the session gate must accept all 375 minutes of 09:15..15:30. \
             Got {in_session} — a count near 45 is the +19,800s IST-origin skew returning."
        );

        // And the tail amnesty must land on the REAL tail (15:28, 15:29), not
        // on 09:58/09:59 as it did under the skew.
        let tail_at = |h: i64, mi: i64| is_tail_minute(stamp(h, mi), origin);
        assert!(tail_at(15, 28), "15:28 must be tail-amnestied");
        assert!(tail_at(15, 29), "15:29 must be tail-amnestied");
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
            production.contains(
                "attempts > 0 && ist_second_of_day_now() >= DEPTH_ATTACH_DEADLINE_IST_SECS"
            ),
            "the depth deadline MUST be guarded by `attempts > 0` — an unguarded check makes a \
             mid-session restart give up before it has looked even once, exactly when the chain \
             table is fullest"
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
