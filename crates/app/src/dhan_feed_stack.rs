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
    DISCONNECT_PACKET_SIZE, FULL_QUOTE_PACKET_SIZE, MARKET_STATUS_PACKET_SIZE, MAX_PLAUSIBLE_LTP,
    OI_PACKET_SIZE, PREVIOUS_CLOSE_PACKET_SIZE, QUOTE_PACKET_SIZE, SPOT_1M_REST_INDICES,
    TICK_PERSIST_END_SECS_OF_DAY_IST, TICKER_PACKET_SIZE,
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
    CapturedFrame, ConnectionSupervisor, PoolSupervisor, RingByteBudget, SubscribeGuard,
    SubscribeGuardRefusal, SubscribeInstrument, WalRingSink, run_connection_with_topup,
};
use tickvault_storage::depth_persistence::{
    DEPTH_KIND_5, DEPTH_KIND_20, DEPTH_KIND_200, DEPTH_SIDE_ASK, DEPTH_SIDE_BID, DepthRow,
    DepthWriter, depth_segment_label,
};
use tickvault_storage::tick_persistence::TickWriter;
use tickvault_storage::ws_frame_spill::{WsFrameSpill, WsType};
use tickvault_trading::candles::multi_tf_aggregator::AGGREGATOR_MAX_SLOTS;
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

    /// Builds the fold, pre-sized for `capacity` instruments so the slot table
    /// and the detector index never realloc mid-session.
    #[must_use]
    pub fn new(writer: TickWriter, capacity: usize) -> Self {
        Self {
            // OFF unless explicitly enabled — see `with_inline_depth`.
            inline_depth: None,
            detector: TickGapDetector::with_capacity(capacity, DetectorConfig::default()),
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
                // Pinned by `tf_index::tests::tf_index_operator_set_is_thirteen`.
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
        let candle_only_refusal =
            (stats.out_of_session || stats.untraded_sentinel) && !hard_refusal;

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
                // Pinned by `tf_index::tests::tf_index_operator_set_is_thirteen`.
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
                // Pinned by `tf_index::tests::tf_index_operator_set_is_thirteen`.
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
) -> DrainOutcome {
    // Whether this drain has ever seen a frame. Owns the up-gauge's rising
    // edge — see the first-frame arm below and the spawn site's correction.
    let mut lane_up = false;
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
                let received_at_nanos = chrono::Utc::now()
                    .timestamp_nanos_opt()
                    .unwrap_or(0)
                    .saturating_sub(queued_nanos);
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
    /// Depth rows appended from the 5 levels carried INLINE in Full-mode tick
    /// packets. Zero unless the ingest was built with
    /// [`LiveIngest::with_inline_depth`] — which the production boot site does
    /// unconditionally. (CORRECTED 2026-08-20: said "unless `[dhan_feed]
    /// persist_full_mode_depth` is on", a key that exists nowhere.)
    pub inline_depth_rows: u64,
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
        return 0;
    };
    // A value above i64::MAX cannot be a real Dhan id. Refuse rather than
    // saturate: saturating writes every such packet under one bogus id.
    let Ok(security_id) = i64::try_from(tick.security_id) else {
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
            }
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
            }
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
            Vec<tickvault_core::websocket::pool_supervisor::SubscribeInstrument>,
        >,
        usize,
    )>,
    // Owned, not borrowed: this runs in its own task, hours after boot.
    ws_audit_tx: tokio::sync::mpsc::Sender<
        tickvault_core::websocket::pool_supervisor::WsLifecycleEvent,
    >,
) {
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
            return;
        }
        // Re-derived every attempt, never hoisted: this task can outlive an
        // IST midnight, and a hoisted date would then query yesterday forever.
        let today_date = crate::dhan_universe::today_ist_date();
        let today_nanos = crate::dhan_universe::ist_midnight_nanos(&today_date);
        let selection =
            crate::dhan_depth_universe::load_depth_universe(&questdb, today_nanos).await;

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
            remaining_main_feed_capacity(main_feed_connections_used)
                .saturating_add(spot_topup.as_ref().map_or(0, |(_, spare)| *spare)),
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
        let pending =
            crate::dhan_contract_universe::stock_options_are_pending(&contracts) && !out_of_time;
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
                        Some((tx, _)) => {
                            // A bounded send that CANNOT block the attach: the
                            // connection task may be mid-frame, and waiting on
                            // it here would stall the depth dial behind a
                            // socket that is doing its job.
                            match tx.try_send(overflow.to_vec()) {
                                Ok(()) => {
                                    spot_topup_used = true;
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
                                // The attach dials its own connections with
                                // their FINAL set — nothing is added to them
                                // later, so they need no top-up channel.
                                out_topups: None,
                            },
                        );
                        contracts_done = true;
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
                            },
                        );
                        depth_done = true;
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
            if contracts_done && depth_done {
                info!(
                    attempts,
                    "late-attach complete: contracts and depth are both on the wire"
                );
                return;
            }
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
    out_topups: Option<&'a mut Vec<(tokio::sync::mpsc::Sender<Vec<SubscribeInstrument>>, usize)>>,
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
        tokio::spawn(async move {
            // Moved in, so the socket's lifetime and the guard's are the same
            // object. Whatever ends this task — a clean return, an early
            // return, or an unwind — the count comes back down.
            let alive = alive;
            let exit = run_connection_with_topup(
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
    .with_inline_depth(DepthIngest::new(&params.questdb));

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
    let drain = tokio::spawn(run_frame_drain(
        frame_rx,
        ingest,
        depth_ingest,
        Arc::clone(&main_feed_budget),
        Arc::clone(&depth_budget),
        Arc::clone(&params.shutdown),
        Arc::clone(&params.feed_health),
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
    let mut main_feed_topups: Vec<(tokio::sync::mpsc::Sender<Vec<SubscribeInstrument>>, usize)> =
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
                    let c = &report.comparison;
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
                    // bounded by construction and queryable; the findings are
                    // already persisted to the audit table, which is where
                    // per-cell detail belongs.
                    info!(
                        targets = targets.len(),
                        outcome = ?c.outcome,
                        instruments = c.instruments,
                        minutes_compared = c.minutes_compared,
                        cells_diverged = c.cells_diverged,
                        missing_live = c.missing_live,
                        missing_rest = c.missing_rest,
                        tail_unsealed = c.tail_unsealed,
                        out_of_session = c.out_of_session,
                        noise_p50_paise = c.noise_p50_paise,
                        noise_p95_paise = c.noise_p95_paise,
                        noise_max_paise = c.noise_max_paise,
                        findings = c.findings.len(),
                        rest_failures = report.rest_failures,
                        malformed_rows = report.malformed_rows,
                        budget_elapsed = report.budget_elapsed,
                        degraded = report.degraded,
                        vacuous = c.is_vacuous(),
                        "Dhan live-feed cross-verification finished — this is the honest \
                         measure of whether the revived feed agrees with Dhan's own record"
                    );
                    if c.is_vacuous() {
                        // A run that compared nothing proves nothing, and the
                        // outcome field alone does not say so loudly enough.
                        error!(
                            code = ErrorCode::WsGapConnectionState.code_str(),
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
        let ids: Vec<u64> = unique.iter().map(|i| u64::from(i.security_id)).collect();
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
        assert_eq!(
            wrapped, 1,
            "expected exactly ONE wrapped ingest.flush() — the single one \
             inside `flush_and_record`. Found {wrapped}: either a flush site \
             bypassed the helper, or the helper was inlined back into the \
             drain (which re-opens the four-sites-to-keep-in-sync problem)"
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
        // Look back over the emit's own argument list only.
        let start = idx.saturating_sub(2_000);
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

    /// The pricing quorum holds CONTRACTS. It must never hold DEPTH — depth
    /// reads the option chain, not spot prices, so waiting for a stock to
    /// print cannot make its instruments arrive any sooner.
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
            .split("let (silent, never) = ingest.scan_silence(now_millis);")
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

    #[test]
    fn the_count_comes_back_down_on_a_panic_not_only_on_a_clean_return() {
        // The defect this exists for: the increment happens OUTSIDE the socket
        // task, so a decrement written as a plain statement at the end of the
        // task body is skipped when the body unwinds. The gauge then reports N
        // sockets alive when N-1 are — permanently, with nothing to correct
        // it, on the one number an operator reads to answer "how many of the
        // sixteen are up?".
        //
        // Serialized against the other test in this module by running both
        // assertions here: `ALIVE_CONNECTIONS` is process-global, and two
        // tests mutating it on parallel threads would make either flaky for a
        // reason unrelated to what they check.
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
}
