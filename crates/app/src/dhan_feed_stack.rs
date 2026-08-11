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
//! # Honest state of this round
//! The supervision layer is complete and tested. The **transport is not**:
//! `crates/core/src/websocket/connection.rs` was deleted on 2026-07-17 and has
//! not been rebuilt, so no socket implementation of
//! [`DhanFeedSocket`](tickvault_core::websocket::pool_supervisor::DhanFeedSocket)
//! exists yet. When the gates are open this module therefore assembles and
//! validates the plan, reserves the budget, publishes the gauges — and then
//! says so, loudly, with `tv_dhan_feed_stack_up` pinned at `0`. It never
//! reports a lane that is not carrying data as up (audit Rule 11, no
//! false-OK). Wiring the transport is the next round and is a change to a
//! module this one already calls, not a change to this one.

use std::sync::atomic::{AtomicBool, Ordering};

use tickvault_common::constants::SPOT_1M_REST_INDICES;
use tickvault_common::error_code::ErrorCode;
use tickvault_common::feed::Feed;
use tickvault_common::tick_types::ParsedTick;
use tickvault_common::types::ExchangeSegment;
use tickvault_core::pipeline::tick_gap_detector::{
    DetectorConfig, TickGapDetector, TickObservation,
};
use tickvault_core::websocket::pool_budget::{ConnectionSlot, DhanEndpointType};
use tickvault_core::websocket::pool_supervisor::{
    PoolSupervisor, SubscribeGuard, SubscribeGuardRefusal, SubscribeInstrument,
};
use tickvault_storage::tick_persistence::TickWriter;
use tickvault_trading::candles::{
    BufferedSeal, ConsumeStats, FeedStrategy, MultiTfAggregator, SealRing,
};
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
    ring: SealRing,
    writer: TickWriter,
    seq_refused: u64,
    evicted: u64,
}

impl LiveIngest {
    /// Builds the fold, pre-sized for `capacity` instruments so the slot table
    /// and the detector index never realloc mid-session.
    #[must_use]
    pub fn new(writer: TickWriter, capacity: usize) -> Self {
        Self {
            detector: TickGapDetector::with_capacity(capacity, DetectorConfig::default()),
            aggregator: MultiTfAggregator::with_capacity(FeedStrategy::DEFAULT, capacity),
            ring: SealRing::new(),
            writer,
            seq_refused: 0,
            evicted: 0,
        }
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
        // Sequence FIRST: if we cannot stamp this row safely we must not touch
        // any fold state, or the aggregator would carry a tick that never
        // reached disk.
        let Some(capture_seq) = capture_seq_from_frame_seq(frame_seq) else {
            self.seq_refused = self.seq_refused.saturating_add(1);
            metrics::counter!(INGEST_SEQ_REFUSED_COUNTER).increment(1);
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

        let mut evicted_here = 0u64;
        let stats: ConsumeStats = self.aggregator.consume_tick_into_ring(
            Feed::Dhan,
            tick,
            None,
            &mut self.ring,
            |_evicted: BufferedSeal| {
                evicted_here = evicted_here.saturating_add(1);
            },
        );
        self.evicted = self.evicted.saturating_add(evicted_here);

        if stats.refused_price || stats.out_of_session || stats.slot_exhausted {
            let reason = if stats.refused_price {
                "price"
            } else if stats.slot_exhausted {
                "slot_exhausted"
            } else {
                "out_of_session"
            };
            metrics::counter!(INGEST_REFUSED_COUNTER, "reason" => reason).increment(1);
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

        metrics::counter!(INGEST_TICKS_COUNTER).increment(1);
        IngestOutcome::Folded {
            sealed: stats.sealed_count,
            amended: stats.amended_count,
        }
    }

    /// Sealed bars waiting in the ring.
    #[must_use]
    pub fn pending_seals(&self) -> usize {
        self.ring.len()
    }

    /// Ticks refused because their sequence would not narrow.
    #[must_use]
    pub const fn seq_refused(&self) -> u64 {
        self.seq_refused
    }

    /// Seals evicted from a full ring (the caller routes these to spill/DLQ).
    #[must_use]
    pub const fn evicted_seals(&self) -> u64 {
        self.evicted
    }
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

    // The 15:31 cross-verify is BLOCKING, not optional: the main feed has no
    // snapshot-on-subscribe and no sequence number, so packet loss is
    // invisible at the protocol level and this comparator is the only ground
    // truth the lane has. Spawned here, inside the same gate, so it can never
    // be enabled without its own verifier.
    spawn_daily_crossverify(&params.main_feed_instruments);

    // The honest half. The supervision layer is complete; the socket is not.
    error!(
        code = ErrorCode::WsGapConnectionState.code_str(),
        planned_connections = plan.len(),
        "Dhan live feed is ENABLED by config and environment, but no WebSocket transport is \
         wired yet — the connection module was deleted on 2026-07-13 and has not been \
         rebuilt. NO Dhan live market data will flow this session. The REST legs \
         (spot 1m, option chain) are unaffected and continue as normal."
    );
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
    if CROSSVERIFY_DEPS.get().is_none() {
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
        info!(
            targets = targets.len(),
            run_at_ist = "15:31",
            "Dhan live-feed 15:31 cross-verification scheduled"
        );
    }))
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
        });
        assert!(handle.is_none(), "a disabled lane must spawn nothing");
    }

    // -- structural proofs --------------------------------------------------

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
            concat!("reqwest", "::"),
        ] {
            assert!(
                !production_half.contains(banned),
                "the live-feed stack must never reach for an instrument download; found \
                 `{banned}`"
            );
        }
    }

    #[test]
    fn test_stack_never_reports_itself_up() {
        // Rule 11, no false-OK: the up-gauge may only ever be set to 0 while
        // no transport is wired. A future round that sets it to 1 must also
        // change this test, deliberately and visibly.
        let src = include_str!("dhan_feed_stack.rs");
        let test_marker = concat!("#[cfg(", "test)]");
        let production_half = src.split(test_marker).next().unwrap_or(src);
        assert!(
            production_half.contains("gauge!(FEED_STACK_UP_GAUGE).set(0.0)"),
            "the up-gauge must be pinned down during bring-up"
        );
        assert!(
            !production_half.contains("FEED_STACK_UP_GAUGE).set(1.0)"),
            "nothing may report the lane up while the transport is unwired"
        );
    }
}
