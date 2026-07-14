//! The ONE supervised cadence runner task (design §8): a sleep-to-event
//! `select!` loop (NO polling tick — decisions fire "the instant") driving
//! the per-minute cycle slots through the gates, fanning executor calls,
//! firing event-driven decisions on data-complete, and honest-skipping at
//! the cutoffs.
//!
//! Honesty notes:
//! - The per-cycle loop is O(requests-per-cycle) = 11 with N fixed —
//!   flagged O(N), NOT claimed O(1) (design §14). It is COLD path (one
//!   cycle per minute); per-cycle allocations (the event vec, the
//!   completion channel) are deliberate and bounded.
//! - Supervision: respawn arms are reachable in unwind (dev/test) builds
//!   only — the release profile sets `panic = "abort"`, so a panicked
//!   runner aborts the process (the TICK-FLUSH-01 honesty precedent).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use chrono::{FixedOffset, NaiveDate, TimeZone, Timelike, Utc};
use tickvault_common::config::CadenceConfig;
use tickvault_common::constants::IST_UTC_OFFSET_SECONDS;
use tickvault_common::error_code::ErrorCode;
use tickvault_common::feed::Feed;
use tickvault_common::trading_calendar::TradingCalendar;
use tokio::sync::{Notify, mpsc};
use tokio::task::JoinHandle;
use tracing::{debug, error, info};

use super::assembly::{
    ChainCell, ChainProvenance, LaneAssembly, MoneynessFold, SpotProvenance, fold_chain_moneyness,
};
use super::decision::{
    CadenceEvent, CadenceState, DecisionLatch, DecisionOutcome, DecisionSnapshot, SkipReason,
    emit_decision, next_cadence_state,
};
use super::executor::{
    CadenceExecutor, CadenceFetchError, ChainFetchOk, ChainFetchRequest, SpotFetchRequest,
    SpotSnapshot, SpotTarget,
};
use super::gate::{DhanGates, GateVerdict};
use super::ladder::{
    CycleVerdict, LadderState, failure_arms_ladder, may_retry_in_cycle, next_rung,
};
use super::schedule::{
    CADENCE_RETRY_LATENCY_ALLOWANCE_MS, CycleSlots, build_cycle_slots, next_joinable_boundary,
};
use crate::pipeline::chain_snapshot::ChainUnderlying;

/// Supervisor respawn backoff (the WS-GAP-05 / SLO-03 house cadence).
pub const CADENCE_RUNNER_RESPAWN_BACKOFF_SECS: u64 = 30;

/// Off-session re-check cadence (trading-day gate + window gate) — the
/// runner sleeps in bounded chunks so shutdown stays responsive and the
/// injected clock stays the single time authority.
pub const CADENCE_OFF_SESSION_POLL_SECS: u64 = 60;

/// Bounded completion-channel capacity: the worst cycle carries 7 Groww
/// burst + 7 Groww fallback + 3 Dhan chains + 3 chain retries + 4 spots +
/// 4 spot retries = 28 completions; 64 gives slack without unboundedness.
pub const CADENCE_COMPLETION_CHANNEL_CAPACITY: usize = 64;

/// Per-request bound on Dhan cadence fetches (mirrors the record-capture
/// legs' 5s request timeout house value).
pub const CADENCE_DHAN_REQUEST_TIMEOUT_MS: i64 = 5_000;

/// A wake later than this past its target is counted + logged loud
/// (CADENCE-03 `late_wake`, coalesced per cycle).
pub const CADENCE_LATE_WAKE_WARN_MS: i64 = 1_000;

/// Dispatch-lateness tolerance for the NOMINAL-denial gate-bug signal: a
/// cycle where ANY Dhan dispatch ran later than this past its slot target
/// demotes the cycle's remaining nominal fires — a gate deferral caused by
/// upstream dispatch lateness (even 1ms of jitter on a previous fire
/// compresses the next wall gap below the monotonic spacing when slot
/// gaps equal the spacing exactly) is EXPECTED deferral behavior, not the
/// should-never schedule/gate consistency bug `gate_deferred_nominal`
/// pages about. ZERO tolerance is the only sound value (replay-proven:
/// any positive band admits sub-band jitter compression as a false
/// gate-bug page); a REAL schedule/gate math bug still surfaces loudly
/// via the `tv_cadence_gate_deferred_total{key}` storm regardless of
/// jitter.
pub const CADENCE_NOMINAL_DISPATCH_TOLERANCE_MS: i64 = 0;

/// The "no timed event pending" sleep bound (the completion channel or
/// the cutoff events wake the loop first in practice).
const CADENCE_IDLE_SLEEP_MS: i64 = 60_000;

// ---------------------------------------------------------------------------
// Clock injection (the runner + tests share one time authority)
// ---------------------------------------------------------------------------

/// The runner's injected time authority: IST wall instants pick TARGETS,
/// the monotonic domain feeds the gates (design §0 "Gate time domain").
pub trait CadenceClock: Send + Sync + 'static {
    /// IST wall-clock milliseconds-of-day.
    fn ist_ms_of_day(&self) -> i64;
    /// IST calendar date (for the trading-day gate + day-start resets).
    fn ist_date(&self) -> NaiveDate;
    /// Monotonic milliseconds (never regresses; feeds the gates).
    fn monotonic_ms(&self) -> i64;
    /// Epoch milliseconds (stamped into executor request deadlines).
    fn epoch_ms(&self) -> i64;
}

/// Production clock: chrono UTC+IST for wall instants,
/// `tokio::time::Instant` since construction for the monotonic domain
/// (paused-time-compatible in tests).
#[derive(Debug)]
pub struct SystemCadenceClock {
    /// The monotonic epoch (task boot).
    boot: tokio::time::Instant,
}

impl SystemCadenceClock {
    /// A fresh clock anchored at "now".
    #[must_use]
    pub fn new() -> Self {
        Self {
            boot: tokio::time::Instant::now(),
        }
    }
}

impl Default for SystemCadenceClock {
    fn default() -> Self {
        Self::new()
    }
}

impl CadenceClock for SystemCadenceClock {
    fn ist_ms_of_day(&self) -> i64 {
        let Some(offset) = FixedOffset::east_opt(IST_UTC_OFFSET_SECONDS) else {
            return 0; // unreachable: IST_UTC_OFFSET_SECONDS is a valid const
        };
        let t = offset.from_utc_datetime(&Utc::now().naive_utc()).time();
        i64::from(t.num_seconds_from_midnight()) * 1_000 + i64::from(t.nanosecond() / 1_000_000)
    }

    fn ist_date(&self) -> NaiveDate {
        let Some(offset) = FixedOffset::east_opt(IST_UTC_OFFSET_SECONDS) else {
            return NaiveDate::MIN; // unreachable: valid const offset
        };
        offset
            .from_utc_datetime(&Utc::now().naive_utc())
            .date_naive()
    }

    fn monotonic_ms(&self) -> i64 {
        // APPROVED: elapsed ms since task boot fits i64 for ~292M years.
        #[allow(clippy::cast_possible_wrap, clippy::cast_possible_truncation)]
        {
            self.boot.elapsed().as_millis() as i64
        }
    }

    fn epoch_ms(&self) -> i64 {
        Utc::now().timestamp_millis()
    }
}

// ---------------------------------------------------------------------------
// Runner wiring
// ---------------------------------------------------------------------------

/// Everything the runner needs (built by `crates/app`'s boot wiring).
pub struct CadenceRunnerDeps<D, G> {
    /// The validated `[cadence]` config.
    pub config: CadenceConfig,
    /// Trading-day calendar.
    pub calendar: Arc<TradingCalendar>,
    /// The Dhan lane executor (the dry-run logger in this PR).
    pub dhan_executor: Arc<D>,
    /// The Groww lane executor (the dry-run logger in this PR).
    pub groww_executor: Arc<G>,
    /// Level-triggered Dhan lane enable flag (read per cycle per lane).
    pub dhan_enabled: Arc<AtomicBool>,
    /// Level-triggered Groww lane enable flag.
    pub groww_enabled: Arc<AtomicBool>,
    /// Graceful-shutdown signal (`notify_waiters` at teardown).
    pub shutdown: Arc<Notify>,
}

impl<D, G> Clone for CadenceRunnerDeps<D, G> {
    fn clone(&self) -> Self {
        Self {
            config: self.config.clone(),
            calendar: Arc::clone(&self.calendar),
            dhan_executor: Arc::clone(&self.dhan_executor),
            groww_executor: Arc::clone(&self.groww_executor),
            dhan_enabled: Arc::clone(&self.dhan_enabled),
            groww_enabled: Arc::clone(&self.groww_enabled),
            shutdown: Arc::clone(&self.shutdown),
        }
    }
}

/// Spawn the supervised cadence runner (the tf_consistency /
/// disk-health-watcher supervision family): the inner loop runs until
/// shutdown; an abnormal exit (unwind-build panic, cancel, unexpected
/// clean return) is classified, counted
/// (`tv_cadence_runner_respawn_total{reason}`), logged as CADENCE-03
/// `stage="respawn"`, backed off and respawned.
// TEST-EXEMPT: tokio supervision shell over the unit-tested pure engine (run_cadence_loop / run_cycle); the spawn site is pinned by crates/app/tests/cadence_boot_wiring_guard.rs and exercised by the dry-run integration test.
pub fn spawn_supervised_cadence_runner<D, G>(deps: CadenceRunnerDeps<D, G>) -> JoinHandle<()>
where
    D: CadenceExecutor + 'static,
    G: CadenceExecutor + 'static,
{
    tokio::spawn(async move {
        loop {
            let clock = Arc::new(SystemCadenceClock::new());
            let inner = tokio::spawn(run_cadence_loop(clock, deps.clone()));
            let reason = match inner.await {
                Ok(LoopExit::Shutdown) => {
                    info!("cadence runner: graceful shutdown — not respawning");
                    return;
                }
                // The inner loop is structurally infinite outside
                // shutdown — a clean return is abnormal.
                Ok(LoopExit::DayLoopBroken) => "clean_exit",
                Err(e) if e.is_panic() => "panic",
                Err(e) if e.is_cancelled() => "cancelled",
                Err(_) => "unknown",
            };
            metrics::counter!("tv_cadence_runner_respawn_total", "reason" => reason).increment(1);
            error!(
                code = ErrorCode::Cadence03SchedulerDegraded.code_str(),
                stage = "respawn",
                reason,
                backoff_secs = CADENCE_RUNNER_RESPAWN_BACKOFF_SECS,
                "CADENCE-03: cadence runner died — respawning after backoff \
                 (unwind-build self-heal; a release panic aborts the process)"
            );
            tokio::select! {
                () = deps.shutdown.notified() => {
                    info!("cadence runner: shutdown during respawn backoff");
                    return;
                }
                () = tokio::time::sleep(Duration::from_secs(CADENCE_RUNNER_RESPAWN_BACKOFF_SECS)) => {}
            }
        }
    })
}

/// Why the inner loop returned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoopExit {
    /// Graceful shutdown was notified.
    Shutdown,
    /// The loop broke without a shutdown (structurally unreachable —
    /// classified `clean_exit` by the supervisor).
    DayLoopBroken,
}

/// The inner day loop: trading-day + window gating, no-mid-cycle-join
/// boundary selection, per-cycle execution, ladder bookkeeping, day-start
/// resets. Generic over the injected clock so the dry-run integration
/// test drives it under paused tokio time.
// TEST-EXEMPT: tokio orchestration over unit-tested pure parts (schedule/gate/ladder/assembly/decision); exercised end-to-end by test_cadence_runner_dry_run_full_cycle_emits_decisions_or_skips.
pub async fn run_cadence_loop<C, D, G>(clock: Arc<C>, deps: CadenceRunnerDeps<D, G>) -> LoopExit
where
    C: CadenceClock,
    D: CadenceExecutor + 'static,
    G: CadenceExecutor + 'static,
{
    let cfg = deps.config.clone();
    let gates = Arc::new(DhanGates::new(
        cfg.chain_min_spacing_ms,
        cfg.dhan_spot_spacing_ms,
    ));
    // Conservative boot re-seed (belt-and-braces beside the structural
    // no-mid-cycle-join rule — design §4 case 4). The FIRST cycle after a
    // reseed demotes nominal gate deferrals: the reseed deliberately
    // holds every gate one full spacing, so a first-cycle deferral is
    // the documented waste-at-most-one-slot behavior, not a gate bug.
    gates.reseed_all(clock.monotonic_ms());
    let mut first_cycle_after_reseed = true;

    let mut ladder = LadderState::default();
    let mut latch = DecisionLatch::new();
    let mut last_boundary: Option<u32> = None;
    let mut current_date = clock.ist_date();
    let mut exhausted_episode = false;
    metrics::gauge!("tv_cadence_ladder_rung").set(f64::from(ladder.rung));

    loop {
        // Day-start reset: rung 0, fresh boundary horizon (design §1).
        let today = clock.ist_date();
        if today != current_date {
            current_date = today;
            if ladder.rung != 0 {
                info!(
                    from_rung = ladder.rung,
                    "cadence: day-start ladder reset to rung 0"
                );
            }
            ladder = LadderState::default();
            exhausted_episode = false;
            last_boundary = None;
            metrics::gauge!("tv_cadence_ladder_rung").set(0.0);
        }

        let now_ms = clock.ist_ms_of_day();
        let is_trading = deps.calendar.is_trading_day(today);
        let boundary = if is_trading {
            next_joinable_boundary(now_ms, last_boundary, ladder.rung, &cfg)
        } else {
            None
        };
        let Some(boundary) = boundary else {
            // Off-session / day over: bounded-chunk sleep re-checking the
            // calendar (shutdown stays responsive).
            tokio::select! {
                () = deps.shutdown.notified() => return LoopExit::Shutdown,
                () = tokio::time::sleep(Duration::from_secs(CADENCE_OFF_SESSION_POLL_SECS)) => {}
            }
            continue;
        };
        // Missed boundaries are counted LOUD (design §4 case 5).
        if let Some(lb) = last_boundary {
            let expected_next = lb.saturating_add(60);
            if boundary > expected_next {
                let missed = (boundary - expected_next) / 60;
                metrics::counter!("tv_cadence_boundary_skipped_total").increment(u64::from(missed));
                error!(
                    code = ErrorCode::Cadence03SchedulerDegraded.code_str(),
                    stage = "boundary_skipped",
                    missed,
                    from_boundary = lb,
                    to_boundary = boundary,
                    "CADENCE-03: cycle boundaries skipped (late wake / \
                     overrun / restart no-mid-cycle-join)"
                );
            }
        }
        last_boundary = Some(boundary);

        let slots = build_cycle_slots(boundary, ladder.rung, &cfg);
        let demote_nominal = first_cycle_after_reseed;
        first_cycle_after_reseed = false;
        let outcome = run_cycle(
            clock.as_ref(),
            &deps,
            &gates,
            &slots,
            &mut latch,
            demote_nominal,
        )
        .await;
        let verdict = match outcome {
            CycleRun::Shutdown => return LoopExit::Shutdown,
            CycleRun::Verdict(v) => v,
        };
        // Ladder bookkeeping (day-scoped, design §3/§7).
        let effective_verdict =
            if verdict == CycleVerdict::DhanFailed && ladder.rung == cfg.dhan_ladder_max_rungs {
                CycleVerdict::FloorExhausted
            } else {
                verdict
            };
        let new_rung = next_rung(ladder.rung, effective_verdict, cfg.dhan_ladder_max_rungs);
        if new_rung != ladder.rung {
            let direction = if new_rung > ladder.rung { "up" } else { "down" };
            metrics::counter!("tv_cadence_ladder_shifts_total", "direction" => direction)
                .increment(1);
            error!(
                code = ErrorCode::Cadence03SchedulerDegraded.code_str(),
                stage = "ladder_shift",
                from_rung = ladder.rung,
                to_rung = new_rung,
                direction,
                "CADENCE-03: Dhan failure ladder shifted (the NEXT cycle's \
                 anchor moves by one step)"
            );
        }
        if effective_verdict == CycleVerdict::FloorExhausted {
            if !exhausted_episode {
                exhausted_episode = true;
                metrics::counter!("tv_cadence_ladder_exhausted_total").increment(1);
                error!(
                    code = ErrorCode::Cadence01LaneDegraded.code_str(),
                    stage = "ladder_exhausted",
                    rung = new_rung,
                    "CADENCE-01: Dhan ladder floor exhausted — cross-source \
                     steady state until the first clean Dhan cycle \
                     (edge-latched per episode)"
                );
            }
        } else if effective_verdict == CycleVerdict::Clean {
            exhausted_episode = false;
        }
        ladder.rung = new_rung;
        metrics::gauge!("tv_cadence_ladder_rung").set(f64::from(ladder.rung));
    }
}

// ---------------------------------------------------------------------------
// One cycle
// ---------------------------------------------------------------------------

/// How one cycle resolved.
enum CycleRun {
    /// Shutdown mid-cycle — drop the cycle, no partial emit (design §7).
    Shutdown,
    /// The whole-cycle ladder verdict.
    Verdict(CycleVerdict),
}

/// One scheduled instant inside the cycle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CycleAction {
    /// A Dhan chain fire (`nominal` = a primary schedule slot).
    DhanChain {
        underlying_idx: usize,
        nominal: bool,
    },
    /// A Dhan spot fire (`nominal` = a primary schedule slot).
    DhanSpot { target_idx: usize, nominal: bool },
    /// The Groww 7-parallel burst.
    GrowwBurst,
    /// The Groww burst-failure verdict + fallback launch.
    GrowwVerdict,
    /// The Groww lane staleness cutoff.
    GrowwCutoff,
    /// The Dhan lane staleness cutoff.
    DhanCutoff,
}

/// A completed fetch, delivered over the bounded completion channel.
struct Completion {
    lane: Feed,
    kind: CompletionKind,
}

enum CompletionKind {
    Chain {
        underlying_idx: usize,
        result: Result<ChainFetchOk, CadenceFetchError>,
    },
    Spot {
        target_idx: usize,
        result: Result<SpotSnapshot, CadenceFetchError>,
    },
}

/// Coalesced per-(lane, cycle) CADENCE-01 stage flags (one emission per
/// lane per cycle, never per-request — design §10).
#[derive(Clone, Copy, Debug, Default)]
struct DegradeFlags {
    fetch_failed: bool,
    rate_limited: bool,
    spot_empty: bool,
    groww_fallback: bool,
    cross_fill: bool,
    chain_embedded_spot: bool,
    moneyness_unknown: bool,
}

impl DegradeFlags {
    fn any(self) -> bool {
        self.fetch_failed
            || self.rate_limited
            || self.spot_empty
            || self.groww_fallback
            || self.cross_fill
            || self.chain_embedded_spot
            || self.moneyness_unknown
    }

    /// Comma-joined stage list (cold path — one small allocation per
    /// degraded lane per cycle, honestly accepted).
    fn stages(self) -> String {
        let mut s = String::new();
        for (flag, name) in [
            (self.fetch_failed, "fetch_failed"),
            (self.rate_limited, "rate_limited"),
            (self.spot_empty, "spot_empty"),
            (self.groww_fallback, "groww_fallback"),
            (self.cross_fill, "cross_fill"),
            (self.chain_embedded_spot, "chain_embedded_spot"),
            (self.moneyness_unknown, "moneyness_unknown"),
        ] {
            if flag {
                if !s.is_empty() {
                    s.push(',');
                }
                s.push_str(name);
            }
        }
        s
    }
}

/// Per-lane in-cycle run state.
struct LaneRun {
    enabled: bool,
    state: CadenceState,
    asm: LaneAssembly,
    resolved: bool,
    flags: DegradeFlags,
    arming_failure: bool,
}

impl LaneRun {
    fn new(feed: Feed, enabled: bool, slots: &CycleSlots) -> Self {
        Self {
            enabled,
            state: CadenceState::Idle,
            asm: LaneAssembly::new(feed, slots.cycle_minute_ist, slots.boundary_ms),
            resolved: false,
            flags: DegradeFlags::default(),
            arming_failure: false,
        }
    }

    /// Drive the FSM, refusing (and debug-asserting on) illegal moves.
    fn fsm(&mut self, event: CadenceEvent) {
        if let Some(next) = next_cadence_state(self.state, event) {
            self.state = next;
        } else {
            debug_assert!(
                false,
                "illegal cadence FSM move: {:?} + {:?}",
                self.state, event
            );
        }
    }
}

/// Gate-key label for the deferred counter (static values only).
const fn chain_gate_key(underlying_idx: usize) -> &'static str {
    match underlying_idx {
        0 => "chain_nifty",
        1 => "chain_banknifty",
        2 => "chain_sensex",
        _ => "chain_unknown",
    }
}

/// Drive ONE cycle end-to-end. Returns the whole-cycle ladder verdict
/// (or Shutdown). The event vec + completion channel are per-cycle,
/// bounded, cold-path allocations.
// APPROVED: the single-cycle event loop is deliberately one function — splitting the select! arms would scatter the lane-state invariants.
#[allow(clippy::too_many_lines)]
async fn run_cycle<C, D, G>(
    clock: &C,
    deps: &CadenceRunnerDeps<D, G>,
    gates: &Arc<DhanGates>,
    slots: &CycleSlots,
    latch: &mut DecisionLatch,
    demote_nominal: bool,
) -> CycleRun
where
    C: CadenceClock,
    D: CadenceExecutor + 'static,
    G: CadenceExecutor + 'static,
{
    let dhan_enabled = deps.dhan_enabled.load(Ordering::Acquire);
    let groww_enabled = deps.groww_enabled.load(Ordering::Acquire);

    let mut cycle = CycleState {
        dhan: LaneRun::new(Feed::Dhan, dhan_enabled, slots),
        groww: LaneRun::new(Feed::Groww, groww_enabled, slots),
        events: Vec::with_capacity(16),
        chain_retries_used: [0; 3],
        spot_retries_used: [0; 4],
        next_chain_retry_slot: 0,
        groww_leg_ok: [false; 7],
        groww_fallback_launched: false,
        late_wake_flagged: false,
        next_spot_retry_target_ms: slots.dhan_spot_slots_ms[3]
            .saturating_add(deps.config.dhan_spot_spacing_ms),
        // Seeded TRUE on the first cycle after a gate reseed — the
        // reseed's one-slot hold is an EXPECTED deferral source.
        dispatch_ran_late: demote_nominal,
    };
    // Anchor FSM arming (level-triggered per cycle per lane).
    if cycle.dhan.enabled {
        cycle.dhan.fsm(CadenceEvent::AnchorReached);
    } else {
        cycle.dhan.fsm(CadenceEvent::OffSessionOrDisabled);
        cycle.dhan.resolved = true;
    }
    if cycle.groww.enabled {
        cycle.groww.fsm(CadenceEvent::AnchorReached);
    } else {
        cycle.groww.fsm(CadenceEvent::OffSessionOrDisabled);
        cycle.groww.resolved = true;
    }
    if cycle.dhan.resolved && cycle.groww.resolved {
        return CycleRun::Verdict(CycleVerdict::Clean);
    }

    if cycle.dhan.enabled {
        for i in 0..ChainUnderlying::COUNT {
            cycle.events.push((
                slots.dhan_chain_slots_ms[i],
                CycleAction::DhanChain {
                    underlying_idx: i,
                    nominal: true,
                },
            ));
        }
        for (k, slot) in slots.dhan_spot_slots_ms.iter().enumerate() {
            cycle.events.push((
                *slot,
                CycleAction::DhanSpot {
                    target_idx: k,
                    nominal: true,
                },
            ));
        }
        cycle
            .events
            .push((slots.dhan_cutoff_ms, CycleAction::DhanCutoff));
    }
    if cycle.groww.enabled {
        cycle
            .events
            .push((slots.groww_burst_ms, CycleAction::GrowwBurst));
        cycle
            .events
            .push((slots.groww_verdict_ms, CycleAction::GrowwVerdict));
        cycle
            .events
            .push((slots.groww_cutoff_ms, CycleAction::GrowwCutoff));
    }
    cycle.events.sort_by_key(|(ms, _)| *ms);

    let (tx, mut rx) = mpsc::channel::<Completion>(CADENCE_COMPLETION_CHANNEL_CAPACITY);

    loop {
        if cycle.dhan.resolved && cycle.groww.resolved && cycle.events.is_empty() {
            break;
        }
        let next_event_at = cycle.events.first().map(|(ms, _)| *ms);
        let now_wall = clock.ist_ms_of_day();
        let sleep_ms = next_event_at.map_or(CADENCE_IDLE_SLEEP_MS, |t| (t - now_wall).max(0));
        // APPROVED: clamped non-negative above — the cast is safe.
        #[allow(clippy::cast_sign_loss)]
        let sleep_dur = Duration::from_millis(sleep_ms as u64);

        tokio::select! {
            () = deps.shutdown.notified() => {
                // Drop the cycle — no partial emit (design §7).
                cycle.dhan.fsm(CadenceEvent::Shutdown);
                cycle.groww.fsm(CadenceEvent::Shutdown);
                return CycleRun::Shutdown;
            }
            Some(completion) = rx.recv() => {
                handle_completion(clock, &deps.config, slots, completion, &mut cycle, latch);
            }
            () = tokio::time::sleep(sleep_dur) => {
                if next_event_at.is_none() {
                    continue;
                }
                let (target_ms, action) = cycle.events.remove(0);
                observe_wake_lateness(clock, target_ms, &mut cycle);
                handle_action(clock, deps, gates, slots, action, &mut cycle, &tx, latch);
            }
        }
    }

    // Cycle wrap-up: coalesced CADENCE-01 per degraded lane + verdict.
    for lane in [&cycle.dhan, &cycle.groww] {
        if lane.enabled && lane.flags.any() {
            error!(
                code = ErrorCode::Cadence01LaneDegraded.code_str(),
                stage = %lane.flags.stages(),
                lane = lane.asm.feed.as_str(),
                cycle_minute_ist = lane.asm.cycle_minute_ist,
                "CADENCE-01: cadence lane degraded this cycle (coalesced)"
            );
        }
    }
    // Rollover only from a lane that ran (a disabled lane parked Idle via
    // OffSessionOrDisabled — Idle + Rollover is deliberately illegal).
    if cycle.dhan.enabled {
        cycle.dhan.fsm(CadenceEvent::Rollover);
    }
    if cycle.groww.enabled {
        cycle.groww.fsm(CadenceEvent::Rollover);
    }
    let verdict = if cycle.dhan.enabled && cycle.dhan.arming_failure {
        CycleVerdict::DhanFailed
    } else {
        CycleVerdict::Clean
    };
    CycleRun::Verdict(verdict)
}

/// The whole per-cycle mutable state, threaded as ONE unit (borrow
/// hygiene for the action/completion dispatchers).
struct CycleState {
    dhan: LaneRun,
    groww: LaneRun,
    events: Vec<(i64, CycleAction)>,
    chain_retries_used: [u32; 3],
    spot_retries_used: [u32; 4],
    next_chain_retry_slot: usize,
    /// Which of the 7 Groww burst legs completed Ok (chains 0..3, spots
    /// 3..7) by the verdict instant.
    groww_leg_ok: [bool; 7],
    groww_fallback_launched: bool,
    late_wake_flagged: bool,
    /// The APPEND grid for Dhan spot retries: starts one spacing after
    /// the LAST nominal spot single, stepping one spacing per scheduled
    /// retry — an appended retry can never contend a nominal slot's gate
    /// window (design §1 "spot retries appended on the 400ms gate").
    next_spot_retry_target_ms: i64,
    /// Latched TRUE the first time a dispatch runs later than
    /// [`CADENCE_NOMINAL_DISPATCH_TOLERANCE_MS`] past its slot target —
    /// subsequent gate deferrals this cycle are EXPECTED (upstream
    /// lateness compressed the wall gap), so they are demoted from the
    /// nominal-denial gate-bug signal.
    dispatch_ran_late: bool,
}

/// Record wake lateness (histogram always; CADENCE-03 once per cycle
/// past the warn threshold).
fn observe_wake_lateness<C: CadenceClock>(clock: &C, target_ms: i64, cycle: &mut CycleState) {
    let lateness = clock.ist_ms_of_day() - target_ms;
    if lateness <= 0 {
        return;
    }
    if lateness > CADENCE_NOMINAL_DISPATCH_TOLERANCE_MS {
        cycle.dispatch_ran_late = true;
    }
    // APPROVED: bounded in-cycle lateness — precision loss is nil.
    #[allow(clippy::cast_precision_loss)]
    metrics::histogram!("tv_cadence_late_wake_ms").record(lateness as f64);
    if lateness > CADENCE_LATE_WAKE_WARN_MS && !cycle.late_wake_flagged {
        cycle.late_wake_flagged = true;
        error!(
            code = ErrorCode::Cadence03SchedulerDegraded.code_str(),
            stage = "late_wake",
            lateness_ms = lateness,
            "CADENCE-03: cadence wake landed late past its slot (coalesced \
             once per cycle)"
        );
    }
}

/// Handle one scheduled instant.
// APPROVED: the action dispatcher threads the whole cycle state — one private fn with one call site; a further split would scatter it.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn handle_action<C, D, G>(
    clock: &C,
    deps: &CadenceRunnerDeps<D, G>,
    gates: &Arc<DhanGates>,
    slots: &CycleSlots,
    action: CycleAction,
    cycle: &mut CycleState,
    tx: &mpsc::Sender<Completion>,
    latch: &mut DecisionLatch,
) where
    C: CadenceClock,
    D: CadenceExecutor + 'static,
    G: CadenceExecutor + 'static,
{
    let now_mono = clock.monotonic_ms();
    let now_wall = clock.ist_ms_of_day();
    match action {
        CycleAction::DhanChain {
            underlying_idx,
            nominal,
        } => {
            if !cycle.dhan.enabled || cycle.dhan.resolved {
                return;
            }
            let underlying = ChainUnderlying::ALL[underlying_idx];
            match gates.try_acquire_chain(underlying, now_mono) {
                GateVerdict::Acquired => {
                    if cycle.dhan.state == CadenceState::Armed {
                        cycle.dhan.fsm(CadenceEvent::FirstFetchDispatched);
                    }
                    let req = ChainFetchRequest {
                        feed: Feed::Dhan,
                        underlying,
                        cycle_minute_ist: slots.cycle_minute_ist,
                        deadline_epoch_ms: clock
                            .epoch_ms()
                            .saturating_add(CADENCE_DHAN_REQUEST_TIMEOUT_MS),
                    };
                    spawn_chain_fetch(
                        Arc::clone(&deps.dhan_executor),
                        tx.clone(),
                        req,
                        underlying_idx,
                        CADENCE_DHAN_REQUEST_TIMEOUT_MS,
                    );
                }
                GateVerdict::RetryAtMs(at_mono) => {
                    defer_action(
                        chain_gate_key(underlying_idx),
                        nominal && !cycle.dispatch_ran_late,
                        at_mono,
                        now_mono,
                        now_wall,
                        action,
                        &mut cycle.events,
                    );
                }
            }
        }
        CycleAction::DhanSpot {
            target_idx,
            nominal,
        } => {
            if !cycle.dhan.enabled || cycle.dhan.resolved {
                return;
            }
            match gates.try_acquire_spot(now_mono) {
                GateVerdict::Acquired => {
                    if cycle.dhan.state == CadenceState::Armed {
                        cycle.dhan.fsm(CadenceEvent::FirstFetchDispatched);
                    }
                    let req = SpotFetchRequest {
                        feed: Feed::Dhan,
                        target: SpotTarget::ALL[target_idx],
                        cycle_minute_ist: slots.cycle_minute_ist,
                        deadline_epoch_ms: clock
                            .epoch_ms()
                            .saturating_add(CADENCE_DHAN_REQUEST_TIMEOUT_MS),
                    };
                    spawn_spot_fetch(
                        Arc::clone(&deps.dhan_executor),
                        tx.clone(),
                        req,
                        target_idx,
                        CADENCE_DHAN_REQUEST_TIMEOUT_MS,
                    );
                }
                GateVerdict::RetryAtMs(at_mono) => {
                    defer_action(
                        "spot",
                        nominal && !cycle.dispatch_ran_late,
                        at_mono,
                        now_mono,
                        now_wall,
                        action,
                        &mut cycle.events,
                    );
                }
            }
        }
        CycleAction::GrowwBurst => {
            if !cycle.groww.enabled || cycle.groww.resolved {
                return;
            }
            cycle.groww.fsm(CadenceEvent::FirstFetchDispatched);
            // ALL 7 in parallel (gate-free lane by construction; design
            // §4: the Groww arms never touch DhanGates).
            for (i, underlying) in ChainUnderlying::ALL.iter().enumerate() {
                let req = ChainFetchRequest {
                    feed: Feed::Groww,
                    underlying: *underlying,
                    cycle_minute_ist: slots.cycle_minute_ist,
                    deadline_epoch_ms: clock
                        .epoch_ms()
                        .saturating_add(deps.config.groww_request_timeout_ms),
                };
                spawn_chain_fetch(
                    Arc::clone(&deps.groww_executor),
                    tx.clone(),
                    req,
                    i,
                    deps.config.groww_request_timeout_ms,
                );
            }
            for (k, target) in SpotTarget::ALL.iter().enumerate() {
                let req = SpotFetchRequest {
                    feed: Feed::Groww,
                    target: *target,
                    cycle_minute_ist: slots.cycle_minute_ist,
                    deadline_epoch_ms: clock
                        .epoch_ms()
                        .saturating_add(deps.config.groww_request_timeout_ms),
                };
                spawn_spot_fetch(
                    Arc::clone(&deps.groww_executor),
                    tx.clone(),
                    req,
                    k,
                    deps.config.groww_request_timeout_ms,
                );
            }
        }
        CycleAction::GrowwVerdict => {
            if !cycle.groww.enabled || cycle.groww.resolved || cycle.groww_fallback_launched {
                return;
            }
            // A leg FAILED iff Err OR still pending at the verdict
            // instant (design §5). Failures re-fetch sequentially:
            // chains first, then spots; successes never re-fetched.
            let failed_chains: Vec<usize> = (0..ChainUnderlying::COUNT)
                .filter(|i| !cycle.groww_leg_ok[*i])
                .collect();
            let failed_spots: Vec<usize> = (0..SpotTarget::ALL.len())
                .filter(|k| !cycle.groww_leg_ok[k + ChainUnderlying::COUNT])
                .collect();
            if failed_chains.is_empty() && failed_spots.is_empty() {
                return;
            }
            cycle.groww_fallback_launched = true;
            cycle.groww.flags.groww_fallback = true;
            for i in &failed_chains {
                metrics::counter!("tv_cadence_groww_fallback_total", "leg" => "chain").increment(1);
                debug!(underlying_idx = i, "cadence: groww chain fallback queued");
            }
            for k in &failed_spots {
                metrics::counter!("tv_cadence_groww_fallback_total", "leg" => "spot").increment(1);
                debug!(target_idx = k, "cadence: groww spot fallback queued");
            }
            let exec = Arc::clone(&deps.groww_executor);
            let fallback_tx = tx.clone();
            let timeout_ms = deps.config.groww_request_timeout_ms;
            let cycle_minute = slots.cycle_minute_ist;
            let deadline_base = clock.epoch_ms();
            // Fire-and-forget by design: the task is bounded by the
            // per-request timeouts and dies with the channel.
            drop(tokio::spawn(async move {
                // Sequential: failed CHAINS first, then failed SPOTS
                // (second-1 / second-2 windows, design §1).
                for i in failed_chains {
                    let req = ChainFetchRequest {
                        feed: Feed::Groww,
                        underlying: ChainUnderlying::ALL[i],
                        cycle_minute_ist: cycle_minute,
                        deadline_epoch_ms: deadline_base.saturating_add(timeout_ms),
                    };
                    let result = bound_chain_fetch(exec.as_ref(), req, timeout_ms).await;
                    if fallback_tx
                        .send(Completion {
                            lane: Feed::Groww,
                            kind: CompletionKind::Chain {
                                underlying_idx: i,
                                result,
                            },
                        })
                        .await
                        .is_err()
                    {
                        return; // cycle ended — receiver gone
                    }
                }
                for k in failed_spots {
                    let req = SpotFetchRequest {
                        feed: Feed::Groww,
                        target: SpotTarget::ALL[k],
                        cycle_minute_ist: cycle_minute,
                        deadline_epoch_ms: deadline_base.saturating_add(timeout_ms),
                    };
                    let result = bound_spot_fetch(exec.as_ref(), req, timeout_ms).await;
                    if fallback_tx
                        .send(Completion {
                            lane: Feed::Groww,
                            kind: CompletionKind::Spot {
                                target_idx: k,
                                result,
                            },
                        })
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
            }));
        }
        CycleAction::GrowwCutoff => {
            let CycleState { dhan, groww, .. } = cycle;
            finalize_lane_at_cutoff(clock, slots, groww, dhan, latch);
        }
        CycleAction::DhanCutoff => {
            let CycleState { dhan, groww, .. } = cycle;
            finalize_lane_at_cutoff(clock, slots, dhan, groww, latch);
        }
    }
}

/// Spawn one bounded chain fetch (fire-and-forget by design: bounded by
/// the per-request timeout; the completion send fails harmlessly once the
/// cycle's receiver is gone).
fn spawn_chain_fetch<E: CadenceExecutor + 'static>(
    exec: Arc<E>,
    tx: mpsc::Sender<Completion>,
    req: ChainFetchRequest,
    underlying_idx: usize,
    timeout_ms: i64,
) {
    let lane = req.feed;
    drop(tokio::spawn(async move {
        let result = bound_chain_fetch(exec.as_ref(), req, timeout_ms).await;
        let _sent = tx
            .send(Completion {
                lane,
                kind: CompletionKind::Chain {
                    underlying_idx,
                    result,
                },
            })
            .await;
    }));
}

/// Spawn one bounded spot fetch (see [`spawn_chain_fetch`]).
fn spawn_spot_fetch<E: CadenceExecutor + 'static>(
    exec: Arc<E>,
    tx: mpsc::Sender<Completion>,
    req: SpotFetchRequest,
    target_idx: usize,
    timeout_ms: i64,
) {
    let lane = req.feed;
    drop(tokio::spawn(async move {
        let result = bound_spot_fetch(exec.as_ref(), req, timeout_ms).await;
        let _sent = tx
            .send(Completion {
                lane,
                kind: CompletionKind::Spot { target_idx, result },
            })
            .await;
    }));
}

/// A gate deferral: reschedule the action at the gate's earliest instant
/// (converted back to the wall domain). A NOMINAL slot deferring is a
/// should-never scheduling-math signal (design §10 `gate_deferred_nominal`
/// + the `tv_cadence_gate_denials_total` must-stay-0 contract).
fn defer_action(
    key: &'static str,
    nominal: bool,
    retry_at_mono: i64,
    now_mono: i64,
    now_wall: i64,
    action: CycleAction,
    events: &mut Vec<(i64, CycleAction)>,
) {
    metrics::counter!("tv_cadence_gate_deferred_total", "key" => key).increment(1);
    if nominal {
        metrics::counter!("tv_cadence_gate_denials_total").increment(1);
        error!(
            code = ErrorCode::Cadence03SchedulerDegraded.code_str(),
            stage = "gate_deferred_nominal",
            key,
            "CADENCE-03: a NOMINAL cadence slot was gate-deferred — the \
             schedule math should make this unreachable (gate-bug signal)"
        );
    }
    let wall_at = now_wall.saturating_add(retry_at_mono.saturating_sub(now_mono));
    let demoted = match action {
        CycleAction::DhanChain { underlying_idx, .. } => CycleAction::DhanChain {
            underlying_idx,
            nominal: false,
        },
        CycleAction::DhanSpot { target_idx, .. } => CycleAction::DhanSpot {
            target_idx,
            nominal: false,
        },
        other => other,
    };
    insert_event(events, wall_at, demoted);
}

/// Insert an event keeping the vec sorted (bounded per-cycle size).
fn insert_event(events: &mut Vec<(i64, CycleAction)>, at_ms: i64, action: CycleAction) {
    let pos = events.partition_point(|(ms, _)| *ms <= at_ms);
    events.insert(pos, (at_ms, action));
}

/// Bound a chain fetch by the per-request timeout (Elapsed → `Timeout`).
async fn bound_chain_fetch<E: CadenceExecutor>(
    exec: &E,
    req: ChainFetchRequest,
    timeout_ms: i64,
) -> Result<ChainFetchOk, CadenceFetchError> {
    // APPROVED: validated > 0 at boot — the cast is safe.
    #[allow(clippy::cast_sign_loss)]
    let dur = Duration::from_millis(timeout_ms.max(1) as u64);
    match tokio::time::timeout(dur, exec.fetch_chain(req)).await {
        Ok(r) => r,
        Err(_elapsed) => Err(CadenceFetchError::Timeout),
    }
}

/// Bound a spot fetch by the per-request timeout (Elapsed → `Timeout`).
async fn bound_spot_fetch<E: CadenceExecutor>(
    exec: &E,
    req: SpotFetchRequest,
    timeout_ms: i64,
) -> Result<SpotSnapshot, CadenceFetchError> {
    // APPROVED: validated > 0 at boot — the cast is safe.
    #[allow(clippy::cast_sign_loss)]
    let dur = Duration::from_millis(timeout_ms.max(1) as u64);
    match tokio::time::timeout(dur, exec.fetch_spot(req)).await {
        Ok(r) => r,
        Err(_elapsed) => Err(CadenceFetchError::Timeout),
    }
}

/// Handle one fetch completion: record, count, retry-policy, and attempt
/// event-driven finalize for BOTH lanes (cross-fill runs inside).
fn handle_completion<C: CadenceClock>(
    clock: &C,
    cfg: &CadenceConfig,
    slots: &CycleSlots,
    completion: Completion,
    cycle: &mut CycleState,
    latch: &mut DecisionLatch,
) {
    let now_wall = clock.ist_ms_of_day();
    let lane_feed = completion.lane;
    let leg_label = match &completion.kind {
        CompletionKind::Chain { .. } => "chain",
        CompletionKind::Spot { .. } => "spot",
    };
    let outcome_label = match &completion.kind {
        CompletionKind::Chain { result, .. } => result
            .as_ref()
            .map_or_else(CadenceFetchError::as_str, |_| "ok"),
        CompletionKind::Spot { result, .. } => result
            .as_ref()
            .map_or_else(CadenceFetchError::as_str, |_| "ok"),
    };
    metrics::counter!(
        "tv_cadence_fetch_total",
        "lane" => lane_feed.as_str(),
        "leg" => leg_label,
        "outcome" => outcome_label
    )
    .increment(1);

    {
        let lane: &mut LaneRun = match lane_feed {
            Feed::Dhan => &mut cycle.dhan,
            Feed::Groww => &mut cycle.groww,
        };
        if lane.resolved {
            // Audit-only late response — the decision is untouched (the
            // data still lands in the assembly + the executor already
            // published any snapshot to the registry — never dropped,
            // never duplicated: first-write-wins).
            metrics::counter!("tv_cadence_late_response_total", "lane" => lane_feed.as_str())
                .increment(1);
        }
        match completion.kind {
            CompletionKind::Chain {
                underlying_idx,
                result,
            } => {
                let underlying = ChainUnderlying::ALL[underlying_idx];
                match result {
                    Ok(ok) => {
                        if lane_feed == Feed::Groww {
                            cycle.groww_leg_ok[underlying_idx] = true;
                        }
                        lane.asm.record_chain(
                            underlying,
                            ChainCell {
                                provenance: ChainProvenance::OwnFetch,
                                fetched_at_ms: now_wall,
                                minute_ist: slots.cycle_minute_ist,
                                embedded_spot: ok.underlying_spot,
                            },
                        );
                    }
                    Err(err) => {
                        record_failure(lane, &err);
                        if lane_feed == Feed::Dhan {
                            let earliest = slots
                                .dhan_chain_retry_slots_ms
                                .get(cycle.next_chain_retry_slot)
                                .copied();
                            if let Some(retry_at) = earliest
                                && may_retry_in_cycle(
                                    &err,
                                    cycle.chain_retries_used[underlying_idx],
                                    cfg.in_cycle_retry_max,
                                    retry_at,
                                    CADENCE_RETRY_LATENCY_ALLOWANCE_MS,
                                    slots.dhan_cutoff_ms,
                                )
                            {
                                cycle.chain_retries_used[underlying_idx] += 1;
                                cycle.next_chain_retry_slot += 1;
                                insert_event(
                                    &mut cycle.events,
                                    retry_at.max(now_wall),
                                    CycleAction::DhanChain {
                                        underlying_idx,
                                        nominal: false,
                                    },
                                );
                            }
                        }
                    }
                }
            }
            CompletionKind::Spot { target_idx, result } => {
                let target = SpotTarget::ALL[target_idx];
                match result {
                    Ok(snap) => {
                        if lane_feed == Feed::Groww {
                            cycle.groww_leg_ok[target_idx + ChainUnderlying::COUNT] = true;
                        }
                        lane.asm.record_spot(
                            target,
                            snap.price,
                            SpotProvenance::OwnFetch,
                            now_wall,
                            snap.source_minute_ist,
                        );
                    }
                    Err(err) => {
                        if lane_feed == Feed::Dhan && err == CadenceFetchError::Empty {
                            // 200-empty: coalesced spot_empty stage; does
                            // NOT arm the ladder (Assumed, design §0).
                            lane.flags.spot_empty = true;
                        }
                        record_failure(lane, &err);
                        // The retry is APPENDED on the 400ms spot gate
                        // AFTER the last nominal spot single (design §1
                        // "spot retries appended") — an appended retry can
                        // never contend a nominal slot's gate window.
                        if lane_feed == Feed::Dhan {
                            let retry_target = cycle.next_spot_retry_target_ms.max(now_wall);
                            if may_retry_in_cycle(
                                &err,
                                cycle.spot_retries_used[target_idx],
                                cfg.in_cycle_retry_max,
                                retry_target,
                                CADENCE_RETRY_LATENCY_ALLOWANCE_MS,
                                slots.dhan_cutoff_ms,
                            ) {
                                cycle.spot_retries_used[target_idx] += 1;
                                cycle.next_spot_retry_target_ms =
                                    retry_target.saturating_add(cfg.dhan_spot_spacing_ms);
                                insert_event(
                                    &mut cycle.events,
                                    retry_target,
                                    CycleAction::DhanSpot {
                                        target_idx,
                                        nominal: false,
                                    },
                                );
                            }
                        }
                    }
                }
            }
        }
    }
    // Event-driven finalize: a decision fires the INSTANT a lane's
    // predicate completes (own data first; the cross-fill +
    // chain-embedded rungs run inside).
    let CycleState { dhan, groww, .. } = cycle;
    finalize_if_complete(clock, slots, dhan, groww, latch);
    finalize_if_complete(clock, slots, groww, dhan, latch);
}

/// Count + classify a fetch failure on its lane.
fn record_failure(lane: &mut LaneRun, err: &CadenceFetchError) {
    lane.flags.fetch_failed = true;
    if matches!(err, CadenceFetchError::RateLimited { .. }) {
        lane.flags.rate_limited = true;
        // A 429 arriving DESPITE the gates is a gate-bug signal — the ONE
        // per-request emission in the taxonomy (rare by construction;
        // design §4).
        error!(
            code = ErrorCode::Cadence01LaneDegraded.code_str(),
            stage = "rate_limited",
            lane = lane.asm.feed.as_str(),
            cycle_minute_ist = lane.asm.cycle_minute_ist,
            "CADENCE-01: broker 429 despite the gates — arms the ladder, \
             never blind-retried (gate-bug signal)"
        );
    }
    if lane.asm.feed == Feed::Dhan && failure_arms_ladder(err) {
        lane.arming_failure = true;
    }
}

/// The finalize core: resolve the cross-fill + chain-embedded provenance
/// rungs, then decide the instant the predicate completes.
fn finalize_if_complete<C: CadenceClock>(
    clock: &C,
    slots: &CycleSlots,
    lane: &mut LaneRun,
    other: &LaneRun,
    latch: &mut DecisionLatch,
) {
    if !lane.enabled || lane.resolved || lane.state != CadenceState::Fetching {
        return;
    }
    let now_wall = clock.ist_ms_of_day();
    let cutoff = if lane.asm.feed == Feed::Dhan {
        slots.dhan_cutoff_ms
    } else {
        slots.groww_cutoff_ms
    };
    if !lane.asm.is_data_complete() {
        // Rung 2: cross-source fill from the other lane's same-cycle data
        // (freshness-checked; valid up to AND INCLUDING the cutoff).
        let (spots, chains) = lane.asm.cross_fill_from(&other.asm, now_wall, cutoff);
        if spots + chains > 0 {
            lane.flags.cross_fill = true;
            let direction = if lane.asm.feed == Feed::Dhan {
                "dhan_from_groww"
            } else {
                "groww_from_dhan"
            };
            metrics::counter!("tv_cadence_cross_fill_total", "direction" => direction)
                .increment(u64::from(spots + chains));
            if spots > 0 {
                metrics::counter!("tv_cadence_spot_fallback_total", "source" => "cross_source")
                    .increment(u64::from(spots));
            }
        }
        // Rung 3: the lane's own chain-embedded spot.
        let embedded = lane.asm.fill_spots_from_chain_embedded(now_wall);
        if embedded > 0 {
            lane.flags.chain_embedded_spot = true;
            metrics::counter!("tv_cadence_spot_fallback_total", "source" => "chain_embedded")
                .increment(u64::from(embedded));
        }
    }
    if !lane.asm.is_data_complete() {
        return;
    }
    decide_lane(clock, slots, lane, latch);
}

/// Emit the lane's decision (Decided / DecidedDegraded / Skipped
/// AllUnknown), exactly once via the latch.
fn decide_lane<C: CadenceClock>(
    clock: &C,
    slots: &CycleSlots,
    lane: &mut LaneRun,
    latch: &mut DecisionLatch,
) {
    let now_wall = clock.ist_ms_of_day();
    let feed = lane.asm.feed;
    let mut folds = [MoneynessFold::default(); ChainUnderlying::COUNT];
    let mut provenance: [Option<SpotProvenance>; ChainUnderlying::COUNT] =
        [None; ChainUnderlying::COUNT];
    for u in ChainUnderlying::ALL {
        let (spot_paise, atm_paise, prov) = lane.asm.spot(*u).map_or((0, 0, None), |s| {
            (s.spot_paise, s.atm_paise, Some(s.provenance))
        });
        let fold = fold_chain_moneyness(feed, *u, spot_paise, atm_paise);
        if fold.unknown > 0 {
            metrics::counter!(
                "tv_cadence_moneyness_unknown_total",
                "lane" => feed.as_str(),
                "underlying" => u.as_str()
            )
            .increment(u64::from(fold.unknown));
            lane.flags.moneyness_unknown = true;
        }
        folds[u.index()] = fold;
        provenance[u.index()] = prov;
    }
    let all_unknown = folds.iter().all(MoneynessFold::all_unknown);
    let outcome = if all_unknown {
        DecisionOutcome::Skipped(SkipReason::AllUnknown)
    } else if lane.asm.any_degraded_provenance() {
        DecisionOutcome::DecidedDegraded
    } else {
        DecisionOutcome::Decided
    };
    if !latch.try_latch(feed, lane.asm.cycle_minute_ist) {
        debug_assert!(false, "cadence decision double-latch attempt");
        lane.resolved = true;
        return;
    }
    // FSM: an all-unknown completion is honest-skipped — nothing USABLE
    // arrived; it rides the Skipped state via the BothSourcesDead arm
    // (the precise reason taxonomy lives on the snapshot).
    match outcome {
        DecisionOutcome::Skipped(_) => lane.fsm(CadenceEvent::BothSourcesDead),
        DecisionOutcome::Decided => {
            lane.fsm(CadenceEvent::PredicateCompleteOwn);
            lane.fsm(CadenceEvent::DecisionEmitted);
        }
        DecisionOutcome::DecidedDegraded => {
            lane.fsm(CadenceEvent::PredicateCompleteDegraded);
            lane.fsm(CadenceEvent::DecisionEmitted);
        }
    }
    emit_decision(&DecisionSnapshot {
        lane: feed,
        cycle_minute_ist: lane.asm.cycle_minute_ist,
        outcome,
        vix_missing: lane.asm.vix_missing(),
        post_close: slots.post_close,
        latency_ms: now_wall.saturating_sub(slots.boundary_ms),
        moneyness: folds,
        spot_provenance: provenance,
    });
    lane.resolved = true;
}

/// Cutoff handling: one final finalize attempt, else HONEST-SKIP with
/// the precise reason — never a late decision (design §5).
fn finalize_lane_at_cutoff<C: CadenceClock>(
    clock: &C,
    slots: &CycleSlots,
    lane: &mut LaneRun,
    other: &mut LaneRun,
    latch: &mut DecisionLatch,
) {
    if !lane.enabled || lane.resolved {
        return;
    }
    // Last chance: the cross-fill window is valid up to AND INCLUDING
    // the cutoff instant.
    finalize_if_complete(clock, slots, lane, other, latch);
    if lane.resolved {
        return;
    }
    let now_wall = clock.ist_ms_of_day();
    // Literally nothing usable on EITHER side ⇒ both_sources_dead; a
    // partially-assembled lane at its cutoff ⇒ cutoff.
    let lane_empty = ChainUnderlying::ALL
        .iter()
        .all(|u| lane.asm.chain(*u).is_none() && lane.asm.spot(*u).is_none());
    let other_empty = ChainUnderlying::ALL
        .iter()
        .all(|u| other.asm.chain(*u).is_none() && other.asm.spot(*u).is_none());
    let reason = if lane_empty && (other_empty || !other.enabled) {
        SkipReason::BothSourcesDead
    } else {
        SkipReason::Cutoff
    };
    if !latch.try_latch(lane.asm.feed, lane.asm.cycle_minute_ist) {
        lane.resolved = true;
        return;
    }
    match reason {
        SkipReason::BothSourcesDead => lane.fsm(CadenceEvent::BothSourcesDead),
        SkipReason::Cutoff | SkipReason::AllUnknown => lane.fsm(CadenceEvent::CutoffElapsed),
    }
    emit_decision(&DecisionSnapshot {
        lane: lane.asm.feed,
        cycle_minute_ist: lane.asm.cycle_minute_ist,
        outcome: DecisionOutcome::Skipped(reason),
        vix_missing: lane.asm.vix_missing(),
        post_close: slots.post_close,
        latency_ms: now_wall.saturating_sub(slots.boundary_ms),
        moneyness: [MoneynessFold::default(); ChainUnderlying::COUNT],
        spot_provenance: [None; ChainUnderlying::COUNT],
    });
    lane.resolved = true;
}
