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

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use secrecy::ExposeSecret;
use tickvault_common::config::QuestDbConfig;
use tickvault_common::constants::{
    DHAN_MAIN_FEED_WS_BASE_URL, DHAN_TWENTY_DEPTH_WS_BASE_URL, DHAN_TWO_HUNDRED_DEPTH_WS_BASE_URL,
    DISCONNECT_PACKET_SIZE, FULL_QUOTE_PACKET_SIZE, MARKET_STATUS_PACKET_SIZE, OI_PACKET_SIZE,
    PREVIOUS_CLOSE_PACKET_SIZE, QUOTE_PACKET_SIZE, SPOT_1M_REST_INDICES, TICKER_PACKET_SIZE,
};
use tickvault_common::error_code::ErrorCode;
use tickvault_common::feed::Feed;
use tickvault_common::tick_types::ParsedTick;
use tickvault_common::types::ExchangeSegment;
use tickvault_core::auth::token_manager::global_token_manager;
use tickvault_core::parser::ParsedFrame;
use tickvault_core::parser::dispatcher::dispatch_frame;
use tickvault_core::pipeline::tick_gap_detector::{
    DetectorConfig, TickGapDetector, TickObservation,
};
use tickvault_core::websocket::connection::{
    DhanFeedSocketImpl, DhanSocketParams, FeedTokenBuffer,
};
use tickvault_core::websocket::pool_budget::{ConnectionSlot, DhanEndpointType};
use tickvault_core::websocket::pool_supervisor::{
    CapturedFrame, ConnectionSupervisor, PoolSupervisor, RingByteBudget, SubscribeGuard,
    SubscribeGuardRefusal, SubscribeInstrument, WalRingSink, run_connection,
};
use tickvault_storage::tick_persistence::TickWriter;
use tickvault_storage::ws_frame_spill::{WsFrameSpill, WsType};
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
pub const FEED_STACK_CONNECTIONS_GAUGE: &str = "tv_dhan_feed_stack_connections";

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
    let per_connection = usize::try_from(endpoint.max_instruments_per_connection())
        .unwrap_or(usize::MAX)
        .max(1);
    let needed = set.len().div_ceil(per_connection);
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

    for shard in set.chunks(per_connection) {
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
    /// The aggregator refused the tick (insane price, out of session, or slot
    /// table exhausted). Nothing was folded.
    AggregatorRefused,
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
            seals_emitted: 0,
            seals_dropped: 0,
            pending_rows: 0,
        }
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
                error!(
                    code = ErrorCode::WsGapConnectionState.code_str(),
                    %err,
                    rows = covered,
                    "live tick flush to QuestDB FAILED — the buffered rows were discarded by \
                     the writer contract and are a counted loss. The raw frames remain in the \
                     write-ahead log and are recoverable by replay."
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
    /// Packets 1..N mint a FRESH sequence from the same process-wide counter.
    /// Those values are globally unique — they can never collide with any frame
    /// sequence — but they are NOT reproducible: a replay that re-folded such a
    /// frame would write duplicate rows for its 2nd..Nth packets. That is the
    /// honest cost, and it is the right way round. A duplicate row is visible,
    /// counted, and removable; a silently-dropped tick is neither. (Today
    /// nothing re-folds from replay — recovery restores frames to the WAL
    /// staging area — so the cost is latent rather than live.)
    ///
    /// Arithmetic on the frame sequence was rejected: the counter is
    /// wall-clock-nanosecond seeded with a `prev + 1` fallback under burst, so
    /// consecutive frames can differ by exactly 1. There is no headroom to
    /// carve a packet index into, and inventing some would trade a visible
    /// duplicate for an invisible collision.
    pub fn ingest_tick_at(
        &mut self,
        tick: &ParsedTick,
        frame_seq: u64,
        packet_index: u32,
        recv_monotonic_millis: u64,
    ) -> IngestOutcome {
        let frame_seq = if packet_index == 0 {
            frame_seq
        } else {
            tickvault_storage::ws_frame_spill::next_frame_seq()
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
        if stats.refused_price
            || stats.out_of_session
            || stats.slot_exhausted
            || stats.refused_timestamp
        {
            let reason = if stats.refused_price {
                "price"
            } else if stats.refused_timestamp {
                "timestamp"
            } else if stats.slot_exhausted {
                "slot_exhausted"
            } else {
                "out_of_session"
            };
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
/// O(1) per frame: one fixed-offset parse, one hash lookup in the gap
/// detector, one hash lookup plus `TF_COUNT` scalar folds in the aggregator,
/// one ILP row append. No heap allocation in steady state.
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

async fn run_frame_drain(
    mut rx: tokio::sync::mpsc::Receiver<CapturedFrame>,
    mut ingest: LiveIngest,
    ring_budget: Arc<RingByteBudget>,
) -> DrainOutcome {
    let mut seen: u64 = 0;
    let mut folded: u64 = 0;
    let mut depth_unconsumed: u64 = 0;
    let mut unparseable: u64 = 0;
    let mut flush_timer = tokio::time::interval(FLUSH_INTERVAL);
    flush_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

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
                ring_budget.release(frame.bytes.len());
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
                        // Captured durably in the WAL, counted here, and NOT
                        // folded: no depth consumer exists yet and the operator
                        // has named no depth instruments. Counting it as its own
                        // outcome keeps it honest — it is neither a tick nor a
                        // parse failure.
                        depth_unconsumed = depth_unconsumed.saturating_add(1);
                        c.depth_unconsumed.increment(1);
                    }
                    DhanEndpointType::OrderUpdate => c.non_tick.increment(1),
                }

                // SIZE trigger. Rows sitting in the ILP buffer have NOT reached
                // QuestDB — an unflushed buffer is not storage, it is a leak
                // with a success counter in front of it.
                if ingest.pending_rows() >= FLUSH_ROW_THRESHOLD {
                    blocking_flush(|| ingest.flush());
                }
                if seen.is_multiple_of(DRAIN_REPORT_EVERY) {
                    publish_fold_depth(&ingest);
                }
            }
            // TIME trigger. Without it, the last rows of a thinly-traded
            // instrument sit unflushed below the size threshold waiting for a
            // next tick which, at the close, never comes.
            _ = flush_timer.tick() => {
                blocking_flush(|| ingest.flush());
                publish_fold_depth(&ingest);
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
    publish_fold_depth(&ingest);
    warn!(
        code = ErrorCode::WsGapConnectionState.code_str(),
        frames = seen,
        final_flush_rows = tail,
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
        unparseable,
    }
}

/// Rows buffered before a flush is forced. At ~150 B/row this is a ~150 KB ILP
/// payload — big enough to amortise the round-trip, small enough that a crash
/// loses well under a second of ticks (and the frames themselves survive in the
/// write-ahead log regardless).
pub const FLUSH_ROW_THRESHOLD: u64 = 1_000;

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
                // `frame.seq` is per-FRAME, but `capture_seq` must be unique
                // per ROW or two ticks in one message would collapse into one
                // under the DEDUP key. The packet index is folded in.
                match ingest.ingest_tick_at(&tick, frame.seq, packets, recv_millis) {
                    IngestOutcome::Folded { .. } => {
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
    /// Depth frames captured and deliberately not folded.
    pub depth_unconsumed: u64,
    /// Packets the parser refused.
    pub unparseable: u64,
}

/// Packets we will walk within one main-feed message before declaring the
/// message malformed. The 1 MiB frame cap over the smallest (16-byte) packet
/// bounds a legitimate message well under this.
pub const MAX_PACKETS_PER_FRAME: u32 = 70_000;

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

    let (frame_tx, frame_rx) = tokio::sync::mpsc::channel::<CapturedFrame>(FRAME_RING_CAPACITY);
    // The ring's second bound. One budget shared by every socket, because the
    // heap it protects is one heap: five main-feed connections each holding a
    // per-pool share would bound five times the memory the host actually has.
    let ring_budget = Arc::new(RingByteBudget::new(FRAME_RING_MAX_BYTES));
    let drain = tokio::spawn(run_frame_drain(frame_rx, ingest, Arc::clone(&ring_budget)));

    // ---- the sockets -------------------------------------------------------
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
            DhanSocketParams::new(endpoint, base_url.to_string(), client_id.clone()),
            current_feed_token,
        );
        let sink = Arc::new(WalRingSink::new(
            Arc::clone(&spill),
            frame_tx.clone(),
            Arc::clone(&ring_budget),
            WsType::LiveFeed,
            endpoint,
        ));
        let guard = planned.guard;
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
            info!(
                endpoint = endpoint.as_str(),
                pool_index = planned.slot.pool_index,
                ?exit,
                "supervised Dhan live-feed connection finished"
            );
        });
        dialed = dialed.saturating_add(1);
    }
    // Drop the template sender: while it lived, the ring could never close, so
    // the drain would hang forever after the last socket died instead of
    // reporting that the lane went dark.
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
    info!(
        dialed,
        seeded,
        ring_capacity = FRAME_RING_CAPACITY,
        // BOTH bounds, because reporting only the frame count is what let a
        // 16 GiB memory ceiling hide behind a 65,536 that looks modest. An
        // operator reading this line should be able to see the queue's real
        // size in the unit that runs out.
        ring_max_bytes = ring_budget.cap(),
        "Dhan 16-connection live feed is up: sockets dialed, frames captured to the WAL before \
         broadcast, and the tick fold is consuming the ring"
    );

    // Hold the task alive with the drain so the stack's JoinHandle reflects the
    // lane's real lifetime rather than completing the instant it finished
    // dialing.
    if let Err(err) = drain.await {
        // The drain is the ONLY consumer of the ring. If it panicked, every
        // socket is still capturing to the WAL but nothing is folding — the
        // exact shape of a lane that looks alive and produces no candles. Say
        // so at ERROR and drop the up-gauge; never let it end quietly.
        error!(
            code = ErrorCode::WsGapConnectionState.code_str(),
            %err,
            "the Dhan live-feed frame drain DIED — frames are still being captured to the \
             write-ahead log but nothing is folding them into candles this session"
        );
        metrics::gauge!(FEED_STACK_UP_GAUGE).set(0.0);
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
    use std::collections::BTreeSet;
    use std::time::Instant;
    use tickvault_common::types::SecurityId;

    fn instruments(n: usize) -> Vec<SubscribeInstrument> {
        (0..n)
            .map(|i| SubscribeInstrument {
                security_id: SecurityId::try_from(i).unwrap_or(SecurityId::MAX),
                segment: ExchangeSegment::NseFno,
            })
            .collect()
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
    fn test_build_feed_stack_plan_shards_the_index_universe_onto_one_connection() {
        let mut pool = PoolSupervisor::new();
        let plan = build_feed_stack_plan(
            &mut pool,
            Instant::now(),
            &hardcoded_index_universe(),
            &[],
            &[],
        )
        .expect("four indices fit on one connection");
        assert_eq!(plan.len(), 1);
        assert_eq!(plan.count_for(DhanEndpointType::MainFeed), 1);
        assert_eq!(plan.count_for(DhanEndpointType::Depth20), 0);
        assert_eq!(plan.count_for(DhanEndpointType::Depth200), 0);
        assert_eq!(pool.total_open(), 1);
    }

    #[test]
    fn test_plan_count_for_shards_at_the_documented_per_connection_caps() {
        let mut pool = PoolSupervisor::new();
        // 12,001 main-feed instruments = 3 connections at 5,000 each.
        // 101 depth-20 instruments = 3 connections at 50 each.
        // 3 depth-200 instruments = 3 connections at 1 each.
        let plan = build_feed_stack_plan(
            &mut pool,
            Instant::now(),
            &instruments(12_001),
            &instruments(101),
            &instruments(3),
        )
        .expect("all three shards fit inside the authorized pools");
        assert_eq!(plan.count_for(DhanEndpointType::MainFeed), 3);
        assert_eq!(plan.count_for(DhanEndpointType::Depth20), 3);
        assert_eq!(plan.count_for(DhanEndpointType::Depth200), 3);
        assert_eq!(plan.len(), 9);
        assert_eq!(pool.total_open(), 9);
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
        // `run_connection` is the call that actually opens a socket. Anchoring
        // on the bare word "dial" matched a doc comment hundreds of lines
        // earlier and made this assertion pass for the wrong reason — the
        // same read-the-prose-not-the-code mistake the wiring guard made.
        let dial = production
            .find("run_connection(socket")
            .expect("the dial site must exist");

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
                Arc::new(RingByteBudget::new(FRAME_RING_MAX_BYTES)),
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
                Arc::new(RingByteBudget::new(FRAME_RING_MAX_BYTES)),
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
}
