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

use chrono::{NaiveDate, TimeZone, Timelike, Utc};
use tickvault_common::config::CadenceConfig;
use tickvault_common::constants::{CADENCE_SPOT_WINDOW_MS, IST_UTC_OFFSET_SECONDS};
use tickvault_common::error_code::ErrorCode;
use tickvault_common::feed::Feed;
use tickvault_common::trading_calendar::{TradingCalendar, ist_offset};
use tokio::sync::{Notify, mpsc};
use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};

use super::assembly::{
    ChainCell, ChainProvenance, LaneAssembly, MoneynessFold, SpotProvenance,
    chain_moneyness_anchor, cross_fill_freshness_floor_ms, fold_chain_cell_moneyness,
};
use super::decision::{
    CadenceEvent, CadenceState, DecisionLatch, DecisionOutcome, DecisionSnapshot, SkipReason,
    emit_decision, may_decide_at_completion, next_cadence_state,
};
use super::executor::{
    CadenceExecutor, CadenceFetchError, ChainFetchOk, ChainFetchRequest, ExpiryListRequest,
    ExpiryResolver, SpotFetchRequest, SpotSnapshot, SpotTarget,
};
use super::expiry::{
    DayLockedExpiryStore, expiry_page_due_after_wave, naive_to_yyyymmdd, next_failed_wave_count,
    policy_for, resolve_policy_expiry,
};
use super::gate::{DhanGates, GateVerdict};
use super::ladder::{
    CADENCE_DHAN_RUNG0_REENTRY_CAP_PER_DAY, DHAN_SHAPE_MAX_STEP, DhanRung0ReentryCap,
    GROWW_SHAPE_MAX_STEP, SPOT_CONCURRENCY_MAX_STEP, StreakLadder, StreakShift,
    failure_arms_ladder, may_retry_in_cycle, min_spot_step_for_cap,
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

/// Bounded in-cycle sleep chunk: every event wait sleeps at most this
/// long, then RE-READS the injected clock and re-validates the target
/// before popping — a backward wall step re-awaits the target on the
/// corrected clock (never an early fire), and a suspend across IST
/// midnight (the ms-of-day wrap) is detected within one chunk instead of
/// wedging the cycle for hours on one stale-computed sleep.
const CADENCE_EVENT_SLEEP_CHUNK_MS: i64 = 5_000;

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
        // The house IST source (`trading_calendar::ist_offset`) — the
        // SAME offset the trading-day gate + the expiry store's day-flip
        // keying use, so the wall targets and the day lock can never
        // disagree about which IST day it is.
        let t = ist_offset()
            .from_utc_datetime(&Utc::now().naive_utc())
            .time();
        i64::from(t.num_seconds_from_midnight()) * 1_000 + i64::from(t.nanosecond() / 1_000_000)
    }

    fn ist_date(&self) -> NaiveDate {
        // IST trading-day identity via the house `trading_calendar`
        // helper — the day-flip source for the day-locked expiry store
        // (NEVER UTC).
        ist_offset()
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
    /// The day-locked expiry lookup SEAM (2026-07-15). Stamped onto
    /// every chain request at build time; the scheduler NEVER guesses.
    /// Production wiring passes the process-global
    /// [`DayLockedExpiryStore`] (its `ExpiryResolver` read facade);
    /// tests keep injecting `StubExpiryResolver`. `dyn` is deliberate
    /// cold-path dispatch (one lookup per chain request, ~6/minute —
    /// never the tick hot path).
    pub expiry_resolver: Arc<dyn ExpiryResolver>,
    /// The day-locked expiry STORE the boot-phase resolution loop writes
    /// (Workstream A, 2026-07-15). `None` = no resolution loop (the
    /// dry-run integration tests, which drive `expiry_resolver` stubs
    /// directly); production wiring passes the SAME process-global store
    /// the resolver facade reads.
    pub expiry_store: Option<Arc<DayLockedExpiryStore>>,
    /// The Dhan gate set (verifier F1(ii), 2026-07-15): production
    /// wiring passes a clone of the PROCESS-GLOBAL registry
    /// (`gate::global_dhan_gates`) so every future Dhan-firing
    /// composition shares ONE budget; tests inject isolated gates.
    pub gates: Arc<DhanGates>,
    /// TRUE when the wired executors are the DRY-RUN loggers (verifier
    /// F10, 2026-07-15): dry-run-shaped degrades (the structural
    /// every-fetch-Empty skips) log at `info!` with `dry_run = true`
    /// instead of the High coded `error!` storm (~1,500 lines/day of
    /// pure noise); REAL executor failures keep the coded `error!`.
    pub dry_run: bool,
    /// Level-triggered Dhan lane enable flag (read per cycle per lane).
    pub dhan_enabled: Arc<AtomicBool>,
    /// Level-triggered Groww lane enable flag.
    pub groww_enabled: Arc<AtomicBool>,
    /// Typed Telegram sink (R6, 2026-07-16): the expiry cross-broker
    /// DISAGREEMENT page (`CadenceExpiryDisagreement`, edge-latched once
    /// per underlying per day) dispatches through this handle. `None` =
    /// log-only (the dry-run integration tests); production wiring
    /// passes the boot `NotificationService`.
    pub notifier: Option<Arc<crate::notification::NotificationService>>,
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
            expiry_resolver: Arc::clone(&self.expiry_resolver),
            expiry_store: self.expiry_store.as_ref().map(Arc::clone),
            gates: Arc::clone(&self.gates),
            dry_run: self.dry_run,
            dhan_enabled: Arc::clone(&self.dhan_enabled),
            groww_enabled: Arc::clone(&self.groww_enabled),
            notifier: self.notifier.as_ref().map(Arc::clone),
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
// TEST-EXEMPT: tokio supervision shell over the unit-tested pure engine (run_cadence_loop / run_cycle); the spawn site is pinned by crates/app/tests/cadence_boot_wiring_guard.rs and the graceful-shutdown supervisor path is exercised by test_cadence_supervisor_graceful_shutdown_not_respawning; the respawn/backoff arms are the WS-GAP-05 house pattern (unwind-build self-heal only — release panic=abort).
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
    // ONE pinned shutdown future for the WHOLE loop (created before any
    // await): `Notify::notify_waiters` carries no permit, so a fresh
    // `notified()` per select iteration can LOSE the one-shot teardown
    // notification when it races a wake — production shutdown would then
    // hang the runner. The pinned future observes a notification fired at
    // ANY instant after this line.
    let shutdown = Arc::clone(&deps.shutdown);
    let shutdown_fut = shutdown.notified();
    tokio::pin!(shutdown_fut);
    // The gate set is INJECTED (F1(ii), 2026-07-15): production wiring
    // passes the process-global registry; tests pass isolated gates.
    let gates = Arc::clone(&deps.gates);
    // Conservative boot re-seed (belt-and-braces beside the structural
    // no-mid-cycle-join rule — design §4 case 4). The FIRST cycle after a
    // reseed demotes nominal gate deferrals: the reseed deliberately
    // holds every gate one full spacing, so a first-cycle deferral is
    // the documented waste-at-most-one-slot behavior, not a gate bug.
    // A RESPAWNED runner reseeds the SHARED global gates into ITS fresh
    // monotonic domain — stale-domain stamps are cleared, never trusted.
    gates.reseed_all(clock.monotonic_ms());
    let mut first_cycle_after_reseed = true;

    // Workstream A (2026-07-15): the pre-market expiry resolution loop —
    // spawned per runner incarnation, ABORTED when this loop returns (the
    // guard's Drop), so a respawned runner never leaks a sibling
    // resolver. The day-locked store itself is process-global, so the
    // respawn RE-READS the day's resolution instead of re-resolving.
    let _expiry_task = deps.expiry_store.as_ref().map(|store| {
        AbortOnDrop(tokio::spawn(run_expiry_resolution_loop(
            Arc::clone(&clock),
            deps.clone(),
            Arc::clone(store),
        )))
    });

    // The Dhan SHAPE ladder (operator directive 2026-07-16 + the
    // same-day corrections): rung 0 = the ALL-7 primary (3 chains + 4
    // spots concurrent in the burst second); rung 1 = the split
    // fallback (chains in second 1, ALL 4 spots in second 2). Same
    // streak thresholds as the concurrency ladders ("tried that
    // multiple times"); dirty = RateLimited ONLY — the sole arming
    // class per the operator's rate-limit-only correction.
    let mut dhan_shape_ladder = StreakLadder::starting_at(0);
    // RS1(b) (2026-07-16): the per-IST-day rung-0 RE-ENTRY cap — the
    // termination belt for the UNVERIFIED-LIVE chain-bucket exemption
    // (rule file §0b). After CADENCE_DHAN_RUNG0_REENTRY_CAP_PER_DAY
    // same-day recoveries to rung 0, the next demotion holds rung 1 for
    // the rest of the session (the day-start reset below re-arms it) —
    // a one-bucket wire can never oscillate the shape 0⇄1 all day.
    let mut dhan_rung0_cap = DhanRung0ReentryCap::default();
    // The adaptive concurrency ladders (operator spec addition 2026-07-15;
    // day-scoped like the shape rung). The Dhan spot ladder starts at
    // the STRUCTURAL floor for the configured window cap (a cap below 4
    // cannot admit the full step-0 simultaneous group).
    let spot_step_floor = min_spot_step_for_cap(cfg.spot_window_cap);
    let mut spot_ladder = StreakLadder::starting_at(spot_step_floor);
    let mut groww_ladder = StreakLadder::starting_at(0);
    let mut latch = DecisionLatch::new();
    let mut last_boundary: Option<u32> = None;
    let mut current_date = clock.ist_date();
    let mut exhausted_episode = false;
    let mut lanes_parked = false;
    metrics::gauge!("tv_cadence_dhan_shape_step").set(f64::from(dhan_shape_ladder.step));
    metrics::gauge!("tv_cadence_spot_concurrency_step").set(f64::from(spot_ladder.step));
    metrics::gauge!("tv_cadence_groww_shape_step").set(f64::from(groww_ladder.step));

    loop {
        // Day-start reset: rung 0, fresh boundary horizon (design §1).
        let today = clock.ist_date();
        if today != current_date {
            current_date = today;
            if dhan_shape_ladder.step != 0 {
                info!(
                    from_step = dhan_shape_ladder.step,
                    "cadence: day-start Dhan shape ladder reset to rung 0"
                );
            }
            dhan_shape_ladder = StreakLadder::starting_at(0);
            // RS1(b): the day-start reset re-arms the rung-0 re-entry cap.
            dhan_rung0_cap = DhanRung0ReentryCap::default();
            spot_ladder = StreakLadder::starting_at(spot_step_floor);
            groww_ladder = StreakLadder::starting_at(0);
            exhausted_episode = false;
            last_boundary = None;
            // The decision latch stores bare minute-of-day slots, which
            // recur EVERY day — a lane whose slot froze across the day
            // flip (parked lanes, a midnight suspend) would otherwise
            // collide on the same minute-of-day tomorrow: try_latch
            // refuses, NO Decided/Skipped is emitted for that
            // (lane, minute) (an exactly-once hole in the zero
            // direction) and the should-never double_latch +
            // illegal_fsm_move channels fire for a routine pattern
            // (hostile-review round 1, CAD-NEW-1/CONC-1, 2026-07-15).
            latch = DecisionLatch::new();
            metrics::gauge!("tv_cadence_dhan_shape_step").set(0.0);
            metrics::gauge!("tv_cadence_spot_concurrency_step").set(f64::from(spot_ladder.step));
            metrics::gauge!("tv_cadence_groww_shape_step").set(0.0);
        }

        // BOTH lanes disabled ⇒ PARK level-triggered WITHOUT consuming
        // boundaries: an instant-resolving all-disabled cycle would
        // otherwise burn every remaining boundary of the day in one tick,
        // silently killing the cadence until the next IST day (a transient
        // dual-disable via /api/feeds must recover at the next real
        // minute). Missed boundaries are counted LOUD on resume by the
        // boundary_skipped check below.
        let dhan_on = deps.dhan_enabled.load(Ordering::Acquire);
        let groww_on = deps.groww_enabled.load(Ordering::Acquire);
        if !dhan_on && !groww_on {
            if !lanes_parked {
                lanes_parked = true;
                info!(
                    "cadence: both lanes disabled — parked (level-triggered \
                     re-check; boundaries not consumed)"
                );
            }
            tokio::select! {
                biased;
                () = &mut shutdown_fut => return LoopExit::Shutdown,
                () = tokio::time::sleep(Duration::from_secs(CADENCE_OFF_SESSION_POLL_SECS)) => {}
            }
            continue;
        }
        if lanes_parked {
            lanes_parked = false;
            info!("cadence: a lane re-enabled — resuming at the next joinable boundary");
        }

        let now_ms = clock.ist_ms_of_day();
        let is_trading = deps.calendar.is_trading_day(today);
        let boundary = if is_trading {
            next_joinable_boundary(now_ms, last_boundary, &cfg)
        } else {
            None
        };
        let Some(boundary) = boundary else {
            // Off-session / day over: bounded-chunk sleep re-checking the
            // calendar (shutdown stays responsive).
            tokio::select! {
                biased;
                () = &mut shutdown_fut => return LoopExit::Shutdown,
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

        let slots = build_cycle_slots(
            boundary,
            dhan_shape_ladder.step,
            spot_ladder.step,
            groww_ladder.step,
            &cfg,
        );
        let demote_nominal = first_cycle_after_reseed;
        first_cycle_after_reseed = false;
        let outcome = run_cycle(
            &clock,
            &deps,
            &gates,
            &slots,
            &mut latch,
            demote_nominal,
            shutdown_fut.as_mut(),
        )
        .await;
        let (dhan_dirty, dhan_spot_dirty, groww_dirty) = match outcome {
            CycleRun::Shutdown => return LoopExit::Shutdown,
            // The IST calendar date changed mid-cycle (suspend across
            // midnight) — the cycle was dropped with no partial emit; the
            // loop top re-reads the date and resets the day state.
            CycleRun::Abandoned => continue,
            CycleRun::Verdict {
                dhan_dirty,
                dhan_spot_dirty,
                groww_dirty,
            } => (dhan_dirty, dhan_spot_dirty, groww_dirty),
        };
        // Adaptive concurrency bookkeeping (2026-07-15): the spot/shape
        // ladders fold their own rate-limit dirty flags — degrade after
        // `concurrency_degrade_after_dirty_cycles` CONSECUTIVE dirty
        // cycles, recover after `concurrency_recover_after_clean_cycles`
        // consecutive clean ones (both Assumed defaults 2/3).
        if let Some(shift) = spot_ladder.advance(
            dhan_spot_dirty,
            cfg.concurrency_degrade_after_dirty_cycles,
            cfg.concurrency_recover_after_clean_cycles,
            spot_step_floor,
            SPOT_CONCURRENCY_MAX_STEP,
        ) {
            log_concurrency_shift(
                "spot_concurrency_shift",
                "tv_cadence_spot_concurrency_shifts_total",
                shift,
                spot_ladder.step,
            );
        }
        metrics::gauge!("tv_cadence_spot_concurrency_step").set(f64::from(spot_ladder.step));
        if let Some(shift) = groww_ladder.advance(
            groww_dirty,
            cfg.concurrency_degrade_after_dirty_cycles,
            cfg.concurrency_recover_after_clean_cycles,
            0,
            GROWW_SHAPE_MAX_STEP,
        ) {
            log_concurrency_shift(
                "groww_shape_shift",
                "tv_cadence_groww_shape_shifts_total",
                shift,
                groww_ladder.step,
            );
        }
        metrics::gauge!("tv_cadence_groww_shape_step").set(f64::from(groww_ladder.step));
        // Dhan SHAPE ladder bookkeeping (day-scoped; operator directive
        // 2026-07-16): a dirty cycle while ALREADY at the split-fallback
        // rung is the exhausted edge (cross-source steady state) —
        // checked BEFORE the streak advance so the shift cycle itself
        // never double-fires the edge.
        let dhan_at_max_before = dhan_shape_ladder.step == DHAN_SHAPE_MAX_STEP;
        if let Some(shift) = dhan_shape_ladder.advance(
            dhan_dirty,
            cfg.concurrency_degrade_after_dirty_cycles,
            cfg.concurrency_recover_after_clean_cycles,
            // RS1(b): once the per-day rung-0 re-entry cap latched, the
            // floor rises to rung 1 (clamp-safe — the latch fires ON a
            // demotion, so the ladder already sits at rung 1).
            dhan_rung0_cap.min_step(),
            DHAN_SHAPE_MAX_STEP,
        ) {
            log_concurrency_shift(
                "dhan_shape_shift",
                "tv_cadence_dhan_shape_shifts_total",
                shift,
                dhan_shape_ladder.step,
            );
            if dhan_rung0_cap.record_shift(shift) {
                // RS1(b): the demotion after the cap-th same-day re-entry
                // to rung 0 — hold the split fallback for the rest of the
                // session. Edge-latched ONCE per IST day by construction
                // (record_shift returns true at most once per day-reset).
                metrics::counter!("tv_cadence_dhan_rung0_reentry_cap_latched_total").increment(1);
                error!(
                    code = ErrorCode::Cadence01LaneDegraded.code_str(),
                    stage = "rung0_reentry_cap_latched",
                    reentry_cap = CADENCE_DHAN_RUNG0_REENTRY_CAP_PER_DAY,
                    "CADENCE-01: Dhan shape ladder rung-0 re-entry cap \
                     latched — holding the split fallback (rung 1) for the \
                     rest of the session (the chain-bucket exemption is \
                     UNVERIFIED-LIVE; a one-bucket wire would otherwise \
                     oscillate the burst shape 0⇄1 all day). The IST \
                     day-start reset re-arms the cap"
                );
            }
        }
        metrics::gauge!("tv_cadence_dhan_shape_step").set(f64::from(dhan_shape_ladder.step));
        let (fire_exhausted, next_episode) =
            exhausted_edge_step(exhausted_episode, dhan_dirty, dhan_at_max_before);
        exhausted_episode = next_episode;
        if fire_exhausted {
            metrics::counter!("tv_cadence_ladder_exhausted_total").increment(1);
            error!(
                code = ErrorCode::Cadence01LaneDegraded.code_str(),
                stage = "ladder_exhausted",
                step = dhan_shape_ladder.step,
                "CADENCE-01: Dhan shape ladder floor exhausted — \
                 cross-source steady state until the first clean Dhan \
                 cycle (edge-latched per episode)"
            );
        }
    }
}

/// The `ladder_exhausted` edge-latch step (rule file §1 `ladder_exhausted`
/// — DHAN-SHAPE-ONLY by construction; RS12 pure test seam, 2026-07-16).
///
/// Inputs: the current episode latch, this cycle's Dhan dirty (rate-
/// limited) verdict, and whether the shape ladder sat at its MAX rung
/// BEFORE this cycle's streak advance (checked pre-advance so the shift
/// cycle itself — the dirty cycle that CAUSES the demotion — never fires
/// the edge). Returns `(fire, next_episode)`:
/// - dirty AT max with the latch off ⇒ fire once + latch (rising edge);
/// - dirty AT max with the latch on ⇒ hold silently (once per episode);
/// - a CLEAN cycle ⇒ re-arm (the next dirty-at-max is a new episode);
/// - dirty but NOT at max ⇒ hold the latch unchanged, never fire.
#[must_use]
pub fn exhausted_edge_step(episode: bool, dhan_dirty: bool, at_max_before: bool) -> (bool, bool) {
    if dhan_dirty && at_max_before {
        (!episode, true)
    } else if !dhan_dirty {
        (false, false)
    } else {
        (false, episode)
    }
}

/// Log + count one streak-ladder shift (2026-07-15 concurrency ladders +
/// the 2026-07-16 Dhan shape ladder — CADENCE-03 self-corrected
/// machinery signal; `direction`: `up` = degraded toward the fallback
/// shape / less concurrency, `down` = recovered toward step 0).
fn log_concurrency_shift(
    stage: &'static str,
    counter_name: &'static str,
    shift: StreakShift,
    to_step: u8,
) {
    let direction = match shift {
        StreakShift::Degraded => "up",
        StreakShift::Recovered => "down",
    };
    metrics::counter!(counter_name, "direction" => direction).increment(1);
    error!(
        code = ErrorCode::Cadence03SchedulerDegraded.code_str(),
        stage = stage,
        to_step,
        direction,
        "CADENCE-03: cadence streak ladder shifted (the NEXT cycle uses \
         the new Dhan shape / spot grouping / Groww fallback shape)"
    );
}

/// Abort-on-drop task guard: the expiry resolution loop dies WITH its
/// runner incarnation (a respawn spawns a fresh one against the same
/// process-global store — re-READ, never re-resolve).
struct AbortOnDrop(JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

// ---------------------------------------------------------------------------
// Pre-market expiry resolution (Workstream A, operator spec 2026-07-15)
// ---------------------------------------------------------------------------

/// The boot-phase + background expiry resolution loop: per
/// (broker, underlying), fetch the vendor expiry list with bounded retry
/// (`expiry_retry_interval_ms`) and record the POLICY date into the
/// day-locked store. The IST deadline
/// (`expiry_deadline_secs_of_day_ist`, default 08:55) gates the
/// edge-latched CADENCE-01 `expiry_unresolved` PAGE — never the attempts:
/// a boot AFTER the deadline still resolves on its first success, and the
/// background retry continues at the same cadence until session end
/// (15:30 IST). Lanes run degraded meanwhile (chains fire
/// `expiry_yyyymmdd = None`).
// TEST-EXEMPT: tokio retry shell over the unit-tested pure policy/store/page primitives (resolve_policy_expiry / DayLockedExpiryStore / expiry_page_due) + the unit-tested resolve_broker_expiries attempt fn; exercised end-to-end by test_cadence_runner_expiry_boot_phase_resolves_and_stamps.
async fn run_expiry_resolution_loop<C, D, G>(
    clock: Arc<C>,
    deps: CadenceRunnerDeps<D, G>,
    store: Arc<DayLockedExpiryStore>,
) where
    C: CadenceClock,
    D: CadenceExecutor + 'static,
    G: CadenceExecutor + 'static,
{
    let interval_ms = deps.config.expiry_retry_interval_ms.max(1);
    let deadline_secs = deps.config.expiry_deadline_secs_of_day_ist;
    // Per-(broker, underlying) page latches, re-armed at the IST day flip
    // (edge-latched: ONE CADENCE-01 `expiry_unresolved` per pair per day).
    let mut paged = [[false; ChainUnderlying::COUNT]; Feed::COUNT];
    let mut paged_day: Option<NaiveDate> = None;
    // ONE pinned shutdown future (the run_cadence_loop lost-notification
    // rationale — notify_waiters carries no permit).
    let shutdown = Arc::clone(&deps.shutdown);
    let shutdown_fut = shutdown.notified();
    tokio::pin!(shutdown_fut);
    // E4 (2026-07-15): was the loop's FIRST observation of the current
    // day already past the deadline (a post-deadline crash-boot)? Such a
    // day requires ≥2 consecutive failed waves before the page fires —
    // never the first-wave hair trigger. R3 (2026-07-15): `failed_waves`
    // counts REAL failed attempt waves PER (broker, underlying) pair —
    // only iterations that actually DISPATCHED a fetch for the pair and
    // left it unresolved advance a cell (`next_failed_wave_count`); a
    // disabled-lane iteration or a gate-deferred/conceded fire never
    // counts (the pre-R3 loop-global counter reached ≥2 with ZERO real
    // attempts on a post-deadline boot with a delayed lane enable, so
    // the FIRST real attempt paged immediately — the E4 hair trigger
    // resurrected via the lane-toggle path).
    let mut booted_after_deadline = false;
    let mut failed_waves = [[0_u32; ChainUnderlying::COUNT]; Feed::COUNT];
    loop {
        // IST trading-day identity via the injected clock (production:
        // `trading_calendar::ist_offset()` — NEVER UTC); the day flip is
        // the ONLY re-resolution trigger (the store re-keys inside
        // record_policy_date / the is_resolved day check).
        let today = clock.ist_date();
        // APPROVED: ms-of-day / 1000 fits u32 (< 86_400) — the cast is safe.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let now_secs = (clock.ist_ms_of_day().max(0) / 1_000) as u32;
        if paged_day != Some(today) {
            paged_day = Some(today);
            paged = [[false; ChainUnderlying::COUNT]; Feed::COUNT];
            booted_after_deadline = now_secs >= deadline_secs;
            failed_waves = [[0_u32; ChainUnderlying::COUNT]; Feed::COUNT];
        }
        let session_over = now_secs >= super::schedule::CADENCE_LAST_CYCLE_BOUNDARY_SECS_OF_DAY_IST;
        if deps.calendar.is_trading_day(today) && !session_over {
            let dhan_on = deps.dhan_enabled.load(Ordering::Acquire);
            let groww_on = deps.groww_enabled.load(Ordering::Acquire);
            let mut attempted = [[false; ChainUnderlying::COUNT]; Feed::COUNT];
            if dhan_on {
                attempted[Feed::Dhan.index()] = resolve_broker_expiries(
                    clock.as_ref(),
                    deps.dhan_executor.as_ref(),
                    store.as_ref(),
                    deps.gates.as_ref(),
                    Feed::Dhan,
                    today,
                    paged[Feed::Dhan.index()],
                    deps.notifier.as_ref(),
                )
                .await;
            }
            if groww_on {
                attempted[Feed::Groww.index()] = resolve_broker_expiries(
                    clock.as_ref(),
                    deps.groww_executor.as_ref(),
                    store.as_ref(),
                    deps.gates.as_ref(),
                    Feed::Groww,
                    today,
                    paged[Feed::Groww.index()],
                    deps.notifier.as_ref(),
                )
                .await;
            }
            // E4 + R3: fold this iteration's wave into the per-pair
            // REAL-failed counters — only pairs whose fetch was actually
            // dispatched AND that stayed unresolved advance.
            for feed in [Feed::Dhan, Feed::Groww] {
                for underlying in ChainUnderlying::ALL {
                    let cell = &mut failed_waves[feed.index()][underlying.index()];
                    *cell = next_failed_wave_count(
                        *cell,
                        attempted[feed.index()][underlying.index()],
                        store.is_resolved(today, feed, *underlying),
                    );
                }
            }
            // The deadline PAGE (edge-latched per pair per day; disabled
            // lanes never page — nothing fires for them).
            for (feed, enabled) in [(Feed::Dhan, dhan_on), (Feed::Groww, groww_on)] {
                if !enabled {
                    continue;
                }
                for underlying in ChainUnderlying::ALL {
                    let resolved = store.is_resolved(today, feed, *underlying);
                    let latch = &mut paged[feed.index()][underlying.index()];
                    if expiry_page_due_after_wave(
                        now_secs,
                        deadline_secs,
                        resolved,
                        *latch,
                        booted_after_deadline,
                        failed_waves[feed.index()][underlying.index()],
                    ) {
                        *latch = true;
                        emit_expiry_deadline_page(deps.dry_run, feed, *underlying, deadline_secs);
                    }
                }
            }
        }
        // R1 (2026-07-15): in the cycle-burst era, retry waves anchor at
        // mid-minute (:30 — `next_expiry_wave_instant_ms`), maximally far
        // from the post-close burst region (2026-07-16: every Dhan +
        // Groww fire packs into T+0..≈T+5s of each minute, retries by
        // ≈T+15s), so a vendor-outage retry cadence can never invade the
        // burst window and evict a NOMINAL fire from the combined
        // per-second budget (a false `gate_deferred_nominal` should-never
        // page every outage minute). The L2 expiry gate stays the
        // backstop. Boot-phase / non-trading / post-session waves keep
        // the plain configured interval — no bursts exist to collide
        // with. The wave itself (above) still runs immediately at spawn:
        // only the SLEEP between waves is anchored. R3-F1 belt (b),
        // 2026-07-15: the anchor decision keys on where the PLAIN
        // `now + interval` target would LAND (`expiry_wave_anchor_active`
        // — pure, schedule.rs), so even if the validate() ≤60s ceiling
        // (belt (a)) ever drifts, the LAST pre-era wake of a >60s
        // interval clamps its FIRST in-era wake to the :30 anchor
        // instead of sleeping straight into the session-entry burst
        // window (65s @ 09:14:58 → 09:16:03, inside the burst grid).
        let now_ms_of_day = clock.ist_ms_of_day();
        let anchor_mid_minute = super::schedule::expiry_wave_anchor_active(
            now_ms_of_day,
            interval_ms,
            deps.calendar.is_trading_day(today),
        );
        let sleep_ms = super::schedule::next_expiry_wave_instant_ms(
            now_ms_of_day,
            interval_ms,
            anchor_mid_minute,
        )
        .saturating_sub(now_ms_of_day)
        .max(1);
        // APPROVED: clamped positive above — the cast is safe.
        #[allow(clippy::cast_sign_loss)]
        let sleep_dur = Duration::from_millis(sleep_ms as u64);
        tokio::select! {
            biased;
            () = &mut shutdown_fut => return,
            () = tokio::time::sleep(sleep_dur) => {}
        }
    }
}

/// Bounded gate-acquire attempts per Dhan expiry fire (verifier L2,
/// 2026-07-15): the resolver sleeps to the carried deferral instant and
/// retries a couple of times before conceding the fire to the next wave.
const CADENCE_EXPIRY_GATE_ACQUIRE_ATTEMPTS: u32 = 3;

/// Deferral-sleep cap per gate-acquire attempt (L2): the expiry spacing
/// + combined window both clear within ~1s of quiet; a carried instant
/// further out than TWO windows means a busy cycle burst — concede the
/// fire to the next wave instead of camping on the budget.
const CADENCE_EXPIRY_GATE_WAIT_CAP_MS: i64 = 2_000;

/// The edge-latched expiry DEADLINE page emit — ONE per (broker,
/// underlying) per IST day; the caller owns the latch, this fn only
/// counts + logs.
///
/// `dry_run` (RS9, 2026-07-16 — the verifier-F10 demotion pattern):
/// under DRY-RUN executors expiry resolution can NEVER succeed (every
/// expiry-list fetch structurally returns `Empty`), so the ~08:55 IST
/// deadline page firing for every enabled (broker, underlying) pair is
/// the EXPECTED shape — 3-6 coded `error!` lines every dry-run day of
/// pure noise. A dry-run deadline page therefore logs at `info!` with a
/// `dry_run = true` field (counter unchanged — the trend survives);
/// REAL executor wirings keep the coded `error!`.
pub fn emit_expiry_deadline_page(
    dry_run: bool,
    broker: Feed,
    underlying: ChainUnderlying,
    deadline_secs: u32,
) {
    metrics::counter!(
        "tv_cadence_expiry_unresolved_total",
        "broker" => broker.as_str(),
        "underlying" => underlying.as_str()
    )
    .increment(1);
    if dry_run {
        info!(
            dry_run = true,
            stage = "expiry_unresolved",
            broker = broker.as_str(),
            underlying = underlying.as_str(),
            deadline_secs_of_day_ist = deadline_secs,
            "cadence expiry deadline passed under DRY-RUN executors \
             (expected shape — resolution cannot succeed on Empty \
             expiry-list fetches; F10/RS9 demotion)"
        );
    } else {
        error!(
            code = ErrorCode::Cadence01LaneDegraded.code_str(),
            stage = "expiry_unresolved",
            broker = broker.as_str(),
            underlying = underlying.as_str(),
            deadline_secs_of_day_ist = deadline_secs,
            "CADENCE-01: expiry unresolved past the pre-market \
             deadline — lanes run degraded (chains fire without \
             an expiry key); background retry continues at the \
             same cadence until session end (edge-latched per \
             broker+underlying per day)"
        );
    }
}

/// One resolution ATTEMPT wave for `broker`: fetch the vendor expiry list
/// for every still-unresolved underlying, apply the pure policy math, and
/// record the day-locked verdict. A `newly_disagreeing` record fires the
/// edge-latched CADENCE-01 `expiry_disagreement` (Dhan WINS for keying
/// BOTH lanes — the store's read facade enforces it; the page carries
/// both raws + the verdict).
///
/// GATING (verifier L2, 2026-07-15): a DHAN expiry-list fire is a Dhan
/// Data-API request — it passes [`DhanGates::try_acquire_expiry`] (the
/// COMBINED per-second budget + the 1-per-rolling-second expiry
/// spacing) BEFORE dispatch, never an ungated REST fire that could
/// stack a cycle burst past Dhan's 5/sec. A deferral sleeps to the
/// carried instant (bounded) and retries; still deferred ⇒ the wave
/// SKIPS the underlying (`tv_cadence_expiry_gate_deferred_total`) and
/// the next `expiry_retry_interval_ms` wave re-attempts — a deferral,
/// never a violation. Groww expiry fires stay ungated by design (no
/// Groww rate rule; the Groww lane never touches [`DhanGates`]).
///
/// RETURNS (R3, 2026-07-15) the per-underlying REAL-attempt flags: TRUE
/// exactly when a fetch was actually DISPATCHED for the pair this wave
/// (gate-deferred/conceded fires and already-resolved skips stay
/// FALSE), so the caller's per-pair failed-wave counters count only
/// real evidence — never loop iterations.
// APPROVED: cold-path resolver wave — the deps are individually threaded (clock/exec/store/gates) so the pure attempt fn stays test-injectable.
#[allow(clippy::too_many_arguments)]
async fn resolve_broker_expiries<C, E>(
    clock: &C,
    exec: &E,
    store: &DayLockedExpiryStore,
    gates: &DhanGates,
    broker: Feed,
    today: NaiveDate,
    paged_row: [bool; ChainUnderlying::COUNT],
    notifier: Option<&Arc<crate::notification::NotificationService>>,
) -> [bool; ChainUnderlying::COUNT]
where
    C: CadenceClock,
    E: CadenceExecutor,
{
    let mut attempted = [false; ChainUnderlying::COUNT];
    let Some(today_yyyymmdd) = naive_to_yyyymmdd(today) else {
        return attempted; // unreachable for market dates (fail-closed)
    };
    for underlying in ChainUnderlying::ALL {
        if store.is_resolved(today, broker, *underlying) {
            continue;
        }
        if broker == Feed::Dhan {
            let mut acquired = false;
            for _ in 0..CADENCE_EXPIRY_GATE_ACQUIRE_ATTEMPTS {
                match gates.try_acquire_expiry(clock.monotonic_ms()) {
                    GateVerdict::Acquired => {
                        acquired = true;
                        break;
                    }
                    GateVerdict::RetryAtMs(at_mono) => {
                        let wait_ms = at_mono.saturating_sub(clock.monotonic_ms());
                        if wait_ms <= 0 {
                            continue;
                        }
                        if wait_ms > CADENCE_EXPIRY_GATE_WAIT_CAP_MS {
                            break;
                        }
                        // APPROVED: clamped positive above — the cast is safe.
                        #[allow(clippy::cast_sign_loss)]
                        tokio::time::sleep(Duration::from_millis(wait_ms as u64)).await;
                    }
                }
            }
            if !acquired {
                metrics::counter!(
                    "tv_cadence_expiry_gate_deferred_total",
                    "broker" => broker.as_str()
                )
                .increment(1);
                debug!(
                    broker = broker.as_str(),
                    underlying = underlying.as_str(),
                    "cadence: expiry-list fire gate-deferred — retrying next \
                     wave (L2: never an ungated Dhan fire)"
                );
                continue;
            }
        }
        // R3: a REAL attempt is being dispatched for this pair.
        attempted[underlying.index()] = true;
        let req = ExpiryListRequest {
            broker,
            underlying: *underlying,
            deadline_epoch_ms: clock
                .epoch_ms()
                .saturating_add(CADENCE_DHAN_REQUEST_TIMEOUT_MS),
        };
        // APPROVED: positive const — the cast is safe.
        #[allow(clippy::cast_sign_loss)]
        let dur = Duration::from_millis(CADENCE_DHAN_REQUEST_TIMEOUT_MS as u64);
        let outcome = match tokio::time::timeout(dur, exec.fetch_expiry_list(req)).await {
            Ok(r) => r,
            Err(_elapsed) => Err(CadenceFetchError::Timeout),
        };
        let outcome_label = match &outcome {
            Ok(_) => "ok",
            Err(e) => e.as_str(),
        };
        metrics::counter!(
            "tv_cadence_expiry_resolution_total",
            "broker" => broker.as_str(),
            "outcome" => outcome_label
        )
        .increment(1);
        let raw_dates = match outcome {
            Ok(dates) => dates,
            Err(err) => {
                if matches!(err, CadenceFetchError::RateLimited { .. }) {
                    // L2 (2026-07-15): an expiry-leg 429 was debug!-only
                    // — now counted + coded loud. For Dhan it arrives
                    // DESPITE the gates (a gate-bug / shared-budget
                    // co-tenant signal, the record_failure precedent);
                    // never blind-retried in-wave — the next interval
                    // wave re-attempts through the gates.
                    metrics::counter!(
                        "tv_cadence_expiry_rate_limited_total",
                        "broker" => broker.as_str()
                    )
                    .increment(1);
                    warn!(
                        code = ErrorCode::Cadence01LaneDegraded.code_str(),
                        stage = "expiry_rate_limited",
                        broker = broker.as_str(),
                        underlying = underlying.as_str(),
                        "CADENCE-01: expiry-list fetch rate-limited — \
                         retrying next wave through the gates (never \
                         blind-retried in-wave)"
                    );
                } else {
                    // Bounded-retry policy: the NEXT interval re-attempts;
                    // the deadline page (not this attempt) is the operator
                    // signal.
                    debug!(
                        broker = broker.as_str(),
                        underlying = underlying.as_str(),
                        outcome = outcome_label,
                        "cadence: expiry-list fetch failed — retrying next interval"
                    );
                }
                continue;
            }
        };
        let Some(date) = resolve_policy_expiry(policy_for(*underlying), &raw_dates, today_yyyymmdd)
        else {
            debug!(
                broker = broker.as_str(),
                underlying = underlying.as_str(),
                raw_count = raw_dates.len(),
                "cadence: expiry list yielded NO policy date (empty / \
                 all-garbage / all-past) — fail-closed, retrying next interval"
            );
            continue;
        };
        let verdict = store.record_policy_date(today, broker, *underlying, date);
        if verdict.recorded {
            info!(
                broker = broker.as_str(),
                underlying = underlying.as_str(),
                expiry = %date.as_iso_string(),
                "cadence: expiry resolved + day-locked"
            );
            // E3 (2026-07-15): the typed FALLING-EDGE recovery signal —
            // fires only when the pre-market deadline page HAD fired for
            // this (broker, underlying) pair (the paged latch). At most
            // once per pair per day by construction (first write wins).
            if paged_row[underlying.index()] {
                metrics::counter!(
                    "tv_cadence_expiry_resolved_late_total",
                    "broker" => broker.as_str(),
                    "underlying" => underlying.as_str()
                )
                .increment(1);
                info!(
                    code = ErrorCode::Cadence01LaneDegraded.code_str(),
                    stage = "expiry_resolved_late",
                    broker = broker.as_str(),
                    underlying = underlying.as_str(),
                    expiry = %date.as_iso_string(),
                    "CADENCE-01 recovery: expiry resolved LATE — after the \
                     pre-market deadline page for this pair; the lanes \
                     re-key from the next fire (falling edge, at most once \
                     per broker+underlying per day)"
                );
            }
        }
        if verdict.newly_disagreeing {
            let view = store.view(today, *underlying);
            metrics::counter!(
                "tv_cadence_expiry_disagreement_total",
                "underlying" => underlying.as_str()
            )
            .increment(1);
            error!(
                code = ErrorCode::Cadence01LaneDegraded.code_str(),
                stage = "expiry_disagreement",
                underlying = underlying.as_str(),
                dhan = view
                    .dhan_raw
                    .map(super::expiry::ExpiryDate::yyyymmdd)
                    .unwrap_or(0),
                groww = view
                    .groww_raw
                    .map(super::expiry::ExpiryDate::yyyymmdd)
                    .unwrap_or(0),
                winner = view
                    .winner
                    .map(super::expiry::ExpiryDate::yyyymmdd)
                    .unwrap_or(0),
                "CADENCE-01: brokers disagree on the policy expiry — Dhan \
                 WINS for keying BOTH lanes (edge-latched per underlying \
                 per day; both raws recorded for provenance)"
            );
            // R6 (2026-07-16): the REAL typed Telegram page — rides the
            // SAME `newly_disagreeing` edge latch (once per underlying
            // per day; the store latches, never per wave). `None` sink =
            // log-only (test wiring); production threads the boot
            // NotificationService. Dated authority row:
            // `dhan-rest-only-noise-lock-2026-07-14.md` §2.2.
            if let Some(sink) = notifier {
                let iso = |d: Option<super::expiry::ExpiryDate>| {
                    d.map_or_else(|| "unknown".to_owned(), |d| d.as_iso_string())
                };
                sink.notify(
                    crate::notification::NotificationEvent::CadenceExpiryDisagreement {
                        underlying: underlying.as_str().to_owned(),
                        dhan_date: iso(view.dhan_raw),
                        groww_date: iso(view.groww_raw),
                    },
                );
            }
        }
    }
    attempted
}

// ---------------------------------------------------------------------------
// One cycle
// ---------------------------------------------------------------------------

/// How one cycle resolved.
enum CycleRun {
    /// Shutdown mid-cycle — drop the cycle, no partial emit (design §7).
    Shutdown,
    /// The IST calendar date changed mid-cycle (suspend across midnight —
    /// the ms-of-day domain wrapped): the cycle is dropped with no
    /// partial emit and no ladder verdict; the day loop resets.
    Abandoned,
    /// The whole-cycle dirty flags: `dhan_dirty` (≥1 RateLimited Dhan
    /// outcome — the SOLE arming class per the operator's 2026-07-16
    /// rate-limit-only correction) feeds the Dhan SHAPE ladder; the
    /// per-broker rate-limit flags feed the 2026-07-15 adaptive
    /// concurrency ladders.
    Verdict {
        dhan_dirty: bool,
        dhan_spot_dirty: bool,
        groww_dirty: bool,
    },
}

/// Which legs one Groww wave fires (the 2026-07-15 three-choice ladder:
/// shape 0 fires all three legs at ONE instant — the classic all-7
/// burst; shapes 1/2 spread them across 1000ms-spaced waves). `pub` so
/// the zero-429 replay proof drives [`build_cycle_events`] DIRECTLY
/// (verifier F3, 2026-07-15) — no runtime consumer outside this module.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GrowwWaveLeg {
    /// All 3 option chains in parallel.
    Chains,
    /// The 3 core underlying spots (NIFTY/BANKNIFTY/SENSEX) in parallel.
    CoreSpots,
    /// The advisory INDIA VIX spot (alone last at choice 3 — deliberately
    /// lowest priority, context-only).
    VixSpot,
}

/// One scheduled instant inside the cycle. `pub` for the F3 replay-proof
/// parity drive of [`build_cycle_events`] (2026-07-15).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CycleAction {
    /// A Dhan chain fire (`nominal` = a primary schedule slot).
    DhanChain {
        underlying_idx: usize,
        nominal: bool,
    },
    /// A Dhan spot fire (`nominal` = a primary schedule slot).
    DhanSpot { target_idx: usize, nominal: bool },
    /// One Groww primary wave (shape 0: three same-instant waves = the
    /// all-7 burst).
    GrowwWave { leg: GrowwWaveLeg },
    /// The Groww wave-failure verdict + fallback launch.
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
    /// A non-Empty failure that ended TERMINAL (its retry budget spent /
    /// no retry admitted) with the cell still missing — the rule-file
    /// `fetch_failed` definition ("after the retry budget"), never a
    /// first-attempt-then-retried-OK blip and never the Empty class.
    fetch_failed: bool,
    rate_limited: bool,
    /// A shared-limiter queue deadline miss (SELF-INFLICTED pacing —
    /// `CadenceFetchError::QueueDelay`, F1(iii) 2026-07-15): its own
    /// coalesced stage, NEVER folded into `fetch_failed` and NEVER
    /// arming the ladders.
    queue_delay: bool,
    /// A spot leg returned 2xx-without-data (either lane).
    spot_empty: bool,
    /// A chain leg returned 2xx-without-usable-data (either lane).
    chain_empty: bool,
    groww_fallback: bool,
    cross_fill: bool,
    chain_embedded_spot: bool,
    moneyness_unknown: bool,
    /// ≥1 chain request was stamped `expiry_yyyymmdd = None` (the
    /// resolver seam is unresolved — the scheduler never guesses; the
    /// executor impl may fall back to its warmup expiry). Always set in
    /// dry-run (the `StubExpiryResolver` is unresolved by design).
    expiry_unresolved: bool,
}

impl DegradeFlags {
    fn any(self) -> bool {
        self.fetch_failed
            || self.rate_limited
            || self.queue_delay
            || self.spot_empty
            || self.chain_empty
            || self.groww_fallback
            || self.cross_fill
            || self.chain_embedded_spot
            || self.moneyness_unknown
            || self.expiry_unresolved
    }

    /// Comma-joined stage list (cold path — one small allocation per
    /// degraded lane per cycle, honestly accepted).
    fn stages(self) -> String {
        let mut s = String::new();
        for (flag, name) in [
            (self.fetch_failed, "fetch_failed"),
            (self.rate_limited, "rate_limited"),
            (self.queue_delay, "queue_delay"),
            (self.spot_empty, "spot_empty"),
            (self.chain_empty, "chain_empty"),
            (self.groww_fallback, "groww_fallback"),
            (self.cross_fill, "cross_fill"),
            (self.chain_embedded_spot, "chain_embedded_spot"),
            (self.moneyness_unknown, "moneyness_unknown"),
            (self.expiry_unresolved, "expiry_unresolved"),
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
    /// Dispatched-but-not-yet-completed OWN fetches (burst + fallback +
    /// retries). Together with the lane's remaining queue events this
    /// decides own-path EXHAUSTION — the fallback rungs (cross-fill +
    /// chain-embedded) may run ONLY once the lane's own path is exhausted
    /// or at its cutoff (design §5 resolution ORDER: own fetch first;
    /// §3(e) cross-source is the fallback steady state, never an
    /// every-cycle preemption of the lane's own scheduled fires).
    inflight: u32,
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
            inflight: 0,
        }
    }

    /// Drive the FSM, refusing illegal moves LOUDLY — the state holds.
    /// Should-never scheduler-logic signal, coded + counted instead of
    /// the pre-fix `debug_assert!(false)` (verifier nuance-b,
    /// 2026-07-15 — the F10 double-latch precedent: a debug_assert
    /// aborted unwind test builds and was SILENT in release).
    fn fsm(&mut self, event: CadenceEvent) {
        if let Some(next) = next_cadence_state(self.state, event) {
            self.state = next;
        } else {
            metrics::counter!(
                "tv_cadence_illegal_fsm_move_total",
                "lane" => self.asm.feed.as_str()
            )
            .increment(1);
            error!(
                code = ErrorCode::Cadence03SchedulerDegraded.code_str(),
                stage = "illegal_fsm_move",
                lane = self.asm.feed.as_str(),
                state = ?self.state,
                event = ?event,
                "CADENCE-03: illegal cadence FSM move REFUSED (state held; \
                 should-never scheduler-logic signal)"
            );
        }
    }
}

/// Build one cycle's SORTED dispatch-order event list from the slot
/// tables — the PURE dispatch-order core the zero-429 replay proof
/// drives DIRECTLY (verifier F3, 2026-07-15: the proptest previously
/// simulated this ordering through a hand-kept mirror; any reorder here
/// now fails the parity assertions instead of drifting silently).
/// Cold-path per-cycle allocation, honestly accepted.
#[must_use]
pub fn build_cycle_events(
    slots: &CycleSlots,
    dhan_enabled: bool,
    groww_enabled: bool,
) -> Vec<(i64, CycleAction)> {
    let mut events: Vec<(i64, CycleAction)> = Vec::with_capacity(16);
    if dhan_enabled {
        for i in 0..ChainUnderlying::COUNT {
            events.push((
                slots.dhan_chain_slots_ms[i],
                CycleAction::DhanChain {
                    underlying_idx: i,
                    nominal: true,
                },
            ));
        }
        for (k, slot) in slots.dhan_spot_slots_ms.iter().enumerate() {
            events.push((
                *slot,
                CycleAction::DhanSpot {
                    target_idx: k,
                    nominal: true,
                },
            ));
        }
        events.push((slots.dhan_cutoff_ms, CycleAction::DhanCutoff));
    }
    if groww_enabled {
        // The shape ladder's waves (shape 0: all three at the burst
        // anchor = the classic all-7 burst; shapes 1/2 spread them).
        events.push((
            slots.groww_chain_wave_ms,
            CycleAction::GrowwWave {
                leg: GrowwWaveLeg::Chains,
            },
        ));
        events.push((
            slots.groww_spot_wave_ms,
            CycleAction::GrowwWave {
                leg: GrowwWaveLeg::CoreSpots,
            },
        ));
        events.push((
            slots.groww_vix_wave_ms,
            CycleAction::GrowwWave {
                leg: GrowwWaveLeg::VixSpot,
            },
        ));
        events.push((slots.groww_verdict_ms, CycleAction::GrowwVerdict));
        events.push((slots.groww_cutoff_ms, CycleAction::GrowwCutoff));
    }
    events.sort_by_key(|(ms, _)| *ms);
    events
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
    clock: &Arc<C>,
    deps: &CadenceRunnerDeps<D, G>,
    gates: &Arc<DhanGates>,
    slots: &CycleSlots,
    latch: &mut DecisionLatch,
    demote_nominal: bool,
    mut shutdown_fut: std::pin::Pin<&mut tokio::sync::futures::Notified<'_>>,
) -> CycleRun
where
    C: CadenceClock,
    D: CadenceExecutor + 'static,
    G: CadenceExecutor + 'static,
{
    let dhan_enabled = deps.dhan_enabled.load(Ordering::Acquire);
    let groww_enabled = deps.groww_enabled.load(Ordering::Acquire);
    let cycle_date = clock.ist_date();

    let mut cycle = CycleState {
        dhan: LaneRun::new(Feed::Dhan, dhan_enabled, slots),
        groww: LaneRun::new(Feed::Groww, groww_enabled, slots),
        events: Vec::new(),
        chain_retries_used: [0; 3],
        spot_retries_used: [0; 4],
        next_chain_retry_slot: 0,
        groww_leg_ok: [false; 7],
        groww_leg_attempts: [0; 7],
        groww_leg_inflight: [false; 7],
        groww_verdict_passed: false,
        groww_fallback_launched: false,
        late_wake_flagged: false,
        skew_flagged: false,
        last_observed_wall: i64::MIN,
        // Spot retries APPEND at the next free window-gate instants after
        // the LAST group: one full window past the last group anchor,
        // stepping one window per appended retry — an appended retry can
        // never contend a nominal group's window budget (2026-07-15 gate
        // change; the pre-window design appended on the 400ms spacing).
        next_spot_retry_target_ms: slots.dhan_spot_slots_ms[3]
            .saturating_add(CADENCE_SPOT_WINDOW_MS),
        // Seeded TRUE on the first cycle after a gate reseed — the
        // reseed's one-slot hold is an EXPECTED deferral source.
        dispatch_ran_late: demote_nominal,
        dhan_spot_dirty: false,
        groww_dirty: false,
        dispatched_any: false,
    };
    // Anchor FSM arming (level-triggered per cycle per lane).
    arm_lane(&mut cycle.dhan);
    arm_lane(&mut cycle.groww);
    if cycle.dhan.resolved && cycle.groww.resolved {
        return CycleRun::Verdict {
            dhan_dirty: false,
            dhan_spot_dirty: false,
            groww_dirty: false,
        };
    }

    cycle.events = build_cycle_events(slots, cycle.dhan.enabled, cycle.groww.enabled);

    let (tx, mut rx) = mpsc::channel::<Completion>(CADENCE_COMPLETION_CHANNEL_CAPACITY);

    loop {
        if cycle.dhan.resolved && cycle.groww.resolved && cycle.events.is_empty() {
            break;
        }
        let next_event_at = cycle.events.first().map(|(ms, _)| *ms);
        let now_wall = clock.ist_ms_of_day();
        // A backward wall step mid-cycle is LOUD (once per cycle): the
        // targets re-await on the corrected clock below — never an early
        // fire in the wall domain; the monotonic gates are unaffected.
        if now_wall < cycle.last_observed_wall && !cycle.skew_flagged {
            cycle.skew_flagged = true;
            error!(
                code = ErrorCode::Cadence03SchedulerDegraded.code_str(),
                stage = "skew_clamped",
                regressed_ms = cycle.last_observed_wall - now_wall,
                "CADENCE-03: wall clock regressed mid-cycle — remaining \
                 targets re-await on the corrected clock (coalesced once \
                 per cycle; monotonic gates unaffected)"
            );
        }
        cycle.last_observed_wall = cycle.last_observed_wall.max(now_wall);
        // Bounded sleep chunk: re-read + re-validate the clock on every
        // wake instead of trusting one stale-computed delta (backward
        // step / suspend-across-midnight defense).
        let sleep_ms = next_event_at
            .map_or(CADENCE_IDLE_SLEEP_MS, |t| (t - now_wall).max(0))
            .min(CADENCE_EVENT_SLEEP_CHUNK_MS);
        // APPROVED: clamped non-negative above — the cast is safe.
        #[allow(clippy::cast_sign_loss)]
        let sleep_dur = Duration::from_millis(sleep_ms as u64);

        tokio::select! {
            biased;
            () = shutdown_fut.as_mut() => {
                // Drop the cycle — no partial emit (design §7).
                cycle.dhan.fsm(CadenceEvent::Shutdown);
                cycle.groww.fsm(CadenceEvent::Shutdown);
                return CycleRun::Shutdown;
            }
            Some(completion) = rx.recv() => {
                // CAD-CORR-1 (hostile round 1, 2026-07-15): the mid-cycle
                // IST-date-change defense must cover the COMPLETION arm
                // too — the biased select drains completions BEFORE the
                // timer arm, so an in-flight fetch resuming after a
                // suspend across IST midnight was processed against the
                // dead day's cycle: the wrapped ms-of-day passed the
                // cutoff guards and could emit a wrong-day decision/skip
                // (with a hugely negative latency sample) before the next
                // timer wake abandoned. The completion is DROPPED (its
                // data belongs to the dead day) and the cycle abandons
                // exactly like the timer arm.
                if clock.ist_date() != cycle_date {
                    // `completion` is deliberately unused here — dropped
                    // with the rest of the dead day's cycle state.
                    abandon_dead_day_cycle(&mut cycle);
                    return CycleRun::Abandoned;
                }
                handle_completion(
                    clock,
                    deps,
                    slots,
                    completion,
                    &mut cycle,
                    latch,
                    &tx,
                );
            }
            () = tokio::time::sleep(sleep_dur) => {
                // Suspend across IST midnight: the ms-of-day domain
                // wrapped, every remaining target belongs to the dead
                // day — drop the cycle (no partial emit) and let the
                // day loop reset.
                if clock.ist_date() != cycle_date {
                    abandon_dead_day_cycle(&mut cycle);
                    return CycleRun::Abandoned;
                }
                // CONC-NEW-1 (hostile round 1, 2026-07-15): while the
                // cycle is PRISTINE (no event popped yet — the day's
                // FIRST cycle is entered near IST midnight and waits
                // ~9h for its 09:16:01 burst), re-observe the runtime
                // lane toggles every wake chunk and RE-ARM the cycle
                // from the fresh flags on any change: lanes re-armed +
                // the event list rebuilt, so a pre-fire `/api/feeds`
                // toggle (either direction) is honored within one ~5s
                // wake chunk instead of being frozen at cycle entry.
                if !cycle.dispatched_any {
                    let dhan_now = deps.dhan_enabled.load(Ordering::Acquire);
                    let groww_now = deps.groww_enabled.load(Ordering::Acquire);
                    if dhan_now != cycle.dhan.enabled || groww_now != cycle.groww.enabled {
                        info!(
                            dhan_enabled = dhan_now,
                            groww_enabled = groww_now,
                            "cadence: runtime lane toggle observed before the \
                             cycle's first fire — cycle re-armed from the \
                             fresh flags"
                        );
                        cycle.dhan = LaneRun::new(Feed::Dhan, dhan_now, slots);
                        cycle.groww = LaneRun::new(Feed::Groww, groww_now, slots);
                        arm_lane(&mut cycle.dhan);
                        arm_lane(&mut cycle.groww);
                        cycle.events = build_cycle_events(slots, dhan_now, groww_now);
                        continue;
                    }
                }
                let Some((target_ms, _)) = cycle.events.first().copied() else {
                    continue;
                };
                if clock.ist_ms_of_day() < target_ms {
                    // Chunked wake / wall regression: the target is not
                    // due yet on the (re-read) clock — never pop early.
                    continue;
                }
                let (target_ms, action) = cycle.events.remove(0);
                cycle.dispatched_any = true;
                observe_wake_lateness(clock.as_ref(), target_ms, &mut cycle);
                handle_action(clock, deps, gates, slots, action, &mut cycle, &tx, latch);
            }
        }
    }

    // Cycle wrap-up: coalesced CADENCE-01 per degraded lane + verdict.
    // F10 (2026-07-15): under DRY-RUN executors every fetch is
    // structurally Empty, so an Empty-shaped degrade every cycle is the
    // EXPECTED shape (~1,500 High error! lines/day of pure noise) —
    // demoted to info! with a dry_run=true field. REAL failure classes
    // (fetch_failed / rate_limited) keep the coded error! even in
    // dry-run (they cannot come from the dry-run executors).
    for lane in [&cycle.dhan, &cycle.groww] {
        if lane.enabled && lane.flags.any() {
            if deps.dry_run && !lane.flags.fetch_failed && !lane.flags.rate_limited {
                info!(
                    dry_run = true,
                    stage = %lane.flags.stages(),
                    lane = lane.asm.feed.as_str(),
                    cycle_minute_ist = lane.asm.cycle_minute_ist,
                    "cadence lane degraded under DRY-RUN executors \
                     (expected shape — F10 demotion)"
                );
            } else {
                error!(
                    code = ErrorCode::Cadence01LaneDegraded.code_str(),
                    stage = %lane.flags.stages(),
                    lane = lane.asm.feed.as_str(),
                    cycle_minute_ist = lane.asm.cycle_minute_ist,
                    "CADENCE-01: cadence lane degraded this cycle (coalesced)"
                );
            }
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
    CycleRun::Verdict {
        dhan_dirty: cycle.dhan.enabled && cycle.dhan.arming_failure,
        dhan_spot_dirty: cycle.dhan_spot_dirty,
        groww_dirty: cycle.groww_dirty,
    }
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
    /// Per-leg Groww completion count (burst = 1st, fallback = 2nd): a
    /// failure is TERMINAL for the leg only on its 2nd attempt (the
    /// verdict — or the L3 DEFERRED per-leg fallback — refetches every
    /// failed leg exactly once).
    groww_leg_attempts: [u8; 7],
    /// Per-leg dispatched-but-not-completed flags (F4, 2026-07-15): the
    /// verdict must NOT refetch a leg whose ORIGINAL request is still in
    /// flight — the pre-fix behavior fired a duplicate concurrent
    /// same-leg request (double request volume on a slow broker). A
    /// still-in-flight leg is SKIPPED by the fallback (await-or-skip;
    /// first-write-wins on completion stays).
    groww_leg_inflight: [bool; 7],
    /// The GrowwVerdict instant passed (F4/L3): a leg completing Err on
    /// its 1st attempt after it was skipped in flight has no later
    /// verdict — the L3 DEFERRED fallback (2026-07-15) dispatches its one
    /// fallback attempt AT that completion when it can still land inside
    /// the lane cutoff; only when it cannot is the leg terminal on its
    /// 1st attempt.
    groww_verdict_passed: bool,
    groww_fallback_launched: bool,
    late_wake_flagged: bool,
    /// Backward-wall-step already logged this cycle (coalesced).
    skew_flagged: bool,
    /// Highest wall instant observed this cycle (regression detector).
    last_observed_wall: i64,
    /// The APPEND grid for Dhan spot retries: starts one FULL WINDOW
    /// after the LAST group anchor, stepping one window per scheduled
    /// retry — an appended retry can never contend a nominal group's
    /// rolling-window budget (2026-07-15 gate change).
    next_spot_retry_target_ms: i64,
    /// Latched TRUE the first time a dispatch runs later than
    /// [`CADENCE_NOMINAL_DISPATCH_TOLERANCE_MS`] past its slot target —
    /// subsequent gate deferrals this cycle are EXPECTED (upstream
    /// lateness compressed the wall gap), so they are demoted from the
    /// nominal-denial gate-bug signal.
    dispatch_ran_late: bool,
    /// ≥1 Dhan SPOT outcome this cycle was RateLimited — feeds the
    /// spot-concurrency ladder's dirty streak (2026-07-15).
    dhan_spot_dirty: bool,
    /// ≥1 Groww leg outcome this cycle was RateLimited — feeds the
    /// fallback-shape ladder's dirty streak (2026-07-15; Assumed — the
    /// coordinator's "persistent failure/rate-limit" read as the
    /// rate-limit class only, mirroring the Dhan spot-dirty rule).
    groww_dirty: bool,
    /// TRUE once the first cycle event has POPPED (CONC-NEW-1, hostile
    /// round 1 2026-07-15): while false the cycle is PRISTINE — nothing
    /// dispatched, no completion possible — so the timer arm may safely
    /// RE-ARM the whole cycle from freshly re-read lane enable flags
    /// (the day's first cycle is entered near IST midnight and waits
    /// ~9h for its burst; the entry snapshot alone froze the
    /// `/api/feeds` toggles for that whole window).
    dispatched_any: bool,
}

/// Arm one lane's FSM from its enable snapshot (`run_cycle` entry + the
/// CONC-NEW-1 pristine re-arm share this): an enabled lane arms at the
/// anchor; a disabled lane parks Idle and resolves immediately.
fn arm_lane(lane: &mut LaneRun) {
    if lane.enabled {
        lane.fsm(CadenceEvent::AnchorReached);
    } else {
        lane.fsm(CadenceEvent::OffSessionOrDisabled);
        lane.resolved = true;
    }
}

/// Mid-cycle IST-date-change abandon (suspend across midnight): the
/// ms-of-day domain wrapped, every remaining target — and every in-flight
/// completion — belongs to the dead day, so the cycle is dropped with NO
/// partial emit and the day loop resets. Shared by the timer arm (the
/// original defense) and the completion arm (CAD-CORR-1, hostile round 1
/// 2026-07-15 — the biased select drains completions FIRST, so the check
/// must exist on both arms or a post-suspend completion is processed
/// against the dead day's cycle).
fn abandon_dead_day_cycle(cycle: &mut CycleState) {
    error!(
        code = ErrorCode::Cadence03SchedulerDegraded.code_str(),
        stage = "skew_clamped",
        "CADENCE-03: IST date changed mid-cycle — cycle \
         abandoned, day loop resets"
    );
    cycle.dhan.fsm(CadenceEvent::Shutdown);
    cycle.groww.fsm(CadenceEvent::Shutdown);
}

/// CONC-NEW-1 (hostile round 1, 2026-07-15): re-observe the runtime lane
/// enable toggles at every dispatch/completion instant. `run_cycle` can
/// be ENTERED long before its first fire, and even mid-day the next
/// cycle's snapshot is taken ~45s before its first fire — the entry
/// snapshot alone let an `/api/feeds` disable keep firing REST requests
/// from a disabled lane. A lane observed DISABLED mid-cycle is dropped
/// like a shutdown: FSM → Idle (no partial emit), resolved,
/// `enabled = false` — no further fires (every dispatch arm re-checks),
/// no degrade page, no Rollover; already-in-flight requests complete as
/// audit-only late responses. The ENABLE direction pre-first-fire is the
/// pristine re-arm in the `run_cycle` timer arm; post-first-fire an
/// enable joins at the next minute boundary.
fn observe_runtime_lane_toggles<D, G>(deps: &CadenceRunnerDeps<D, G>, cycle: &mut CycleState)
where
    D: CadenceExecutor + 'static,
    G: CadenceExecutor + 'static,
{
    if cycle.dhan.enabled && !deps.dhan_enabled.load(Ordering::Acquire) {
        drop_lane_runtime_disabled(&mut cycle.dhan);
    }
    if cycle.groww.enabled && !deps.groww_enabled.load(Ordering::Acquire) {
        drop_lane_runtime_disabled(&mut cycle.groww);
    }
}

/// Drop one lane mid-cycle after its runtime toggle flipped OFF
/// (CONC-NEW-1): shutdown-shaped — never a partial emit, never a
/// degrade page for a deliberately disabled lane.
fn drop_lane_runtime_disabled(lane: &mut LaneRun) {
    info!(
        lane = lane.asm.feed.as_str(),
        cycle_minute_ist = lane.asm.cycle_minute_ist,
        "cadence: lane runtime-disabled mid-cycle — remaining fires \
         dropped (no partial emit; in-flight requests complete audit-only)"
    );
    lane.fsm(CadenceEvent::Shutdown);
    lane.resolved = true;
    lane.enabled = false;
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
    clock: &Arc<C>,
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
    // CONC-NEW-1: dispatch-time re-read of the runtime lane toggles — a
    // lane disabled via /api/feeds after cycle entry must not fire.
    observe_runtime_lane_toggles(deps, cycle);
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
            // ExpiryResolver seam (2026-07-15): resolve BEFORE the gate
            // acquire so the per-(underlying, expiry) stamp keys the
            // authorization (F1(i)); `None` = unresolved — the scheduler
            // NEVER guesses (the executor impl may fall back to its
            // warmup expiry) and the expiry-less fire rides the strictly
            // MORE conservative per-underlying gate alone (subsumption).
            let expiry_yyyymmdd =
                deps.expiry_resolver
                    .resolved_expiry(Feed::Dhan, underlying, clock.ist_date());
            match gates.try_acquire_chain(underlying, expiry_yyyymmdd, now_mono) {
                GateVerdict::Acquired => {
                    if cycle.dhan.state == CadenceState::Armed {
                        cycle.dhan.fsm(CadenceEvent::FirstFetchDispatched);
                    }
                    // The lane's coalesced CADENCE-01 carries the
                    // `expiry_unresolved` stage only for DISPATCHED
                    // expiry-less fires (a deferred fire re-resolves at
                    // its retry instant).
                    if expiry_yyyymmdd.is_none() {
                        cycle.dhan.flags.expiry_unresolved = true;
                    }
                    let req = ChainFetchRequest {
                        feed: Feed::Dhan,
                        underlying,
                        expiry_yyyymmdd,
                        cycle_minute_ist: slots.cycle_minute_ist,
                        deadline_epoch_ms: clock
                            .epoch_ms()
                            .saturating_add(CADENCE_DHAN_REQUEST_TIMEOUT_MS),
                    };
                    cycle.dhan.inflight = cycle.dhan.inflight.saturating_add(1);
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
                    cycle.dhan.inflight = cycle.dhan.inflight.saturating_add(1);
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
        CycleAction::GrowwWave { leg } => {
            if !cycle.groww.enabled || cycle.groww.resolved {
                return;
            }
            if cycle.groww.state == CadenceState::Armed {
                cycle.groww.fsm(CadenceEvent::FirstFetchDispatched);
            }
            // Per-wave dispatch (gate-free lane by construction; design
            // §4: the Groww arms never touch DhanGates). Shape 0 places
            // all three waves at ONE instant (= the classic all-7 burst);
            // shapes 1/2 spread them across 1000ms-spaced wave anchors
            // (the 2026-07-15 three-choice fallback-shape ladder — at
            // choice 2 CoreSpots + VixSpot share the :02 anchor, so ALL
            // 4 spots still fire together).
            match leg {
                GrowwWaveLeg::Chains => {
                    for (i, underlying) in ChainUnderlying::ALL.iter().enumerate() {
                        // ExpiryResolver seam (2026-07-15): `None` =
                        // unresolved — never guessed; coalesced
                        // `expiry_unresolved` stage on the lane.
                        let expiry_yyyymmdd = deps.expiry_resolver.resolved_expiry(
                            Feed::Groww,
                            *underlying,
                            clock.ist_date(),
                        );
                        if expiry_yyyymmdd.is_none() {
                            cycle.groww.flags.expiry_unresolved = true;
                        }
                        let req = ChainFetchRequest {
                            feed: Feed::Groww,
                            underlying: *underlying,
                            expiry_yyyymmdd,
                            cycle_minute_ist: slots.cycle_minute_ist,
                            deadline_epoch_ms: clock
                                .epoch_ms()
                                .saturating_add(deps.config.groww_request_timeout_ms),
                        };
                        cycle.groww.inflight = cycle.groww.inflight.saturating_add(1);
                        cycle.groww_leg_inflight[i] = true;
                        spawn_chain_fetch(
                            Arc::clone(&deps.groww_executor),
                            tx.clone(),
                            req,
                            i,
                            deps.config.groww_request_timeout_ms,
                        );
                    }
                }
                GrowwWaveLeg::CoreSpots | GrowwWaveLeg::VixSpot => {
                    let want_vix = leg == GrowwWaveLeg::VixSpot;
                    for (k, target) in SpotTarget::ALL.iter().enumerate() {
                        if target.chain_underlying().is_none() != want_vix {
                            continue;
                        }
                        let req = SpotFetchRequest {
                            feed: Feed::Groww,
                            target: *target,
                            cycle_minute_ist: slots.cycle_minute_ist,
                            deadline_epoch_ms: clock
                                .epoch_ms()
                                .saturating_add(deps.config.groww_request_timeout_ms),
                        };
                        cycle.groww.inflight = cycle.groww.inflight.saturating_add(1);
                        cycle.groww_leg_inflight[k + ChainUnderlying::COUNT] = true;
                        spawn_spot_fetch(
                            Arc::clone(&deps.groww_executor),
                            tx.clone(),
                            req,
                            k,
                            deps.config.groww_request_timeout_ms,
                        );
                    }
                }
            }
        }
        CycleAction::GrowwVerdict => {
            if !cycle.groww.enabled || cycle.groww.resolved || cycle.groww_fallback_launched {
                return;
            }
            cycle.groww_verdict_passed = true;
            // A leg is refetched iff it FAILED (Err completed) — never a
            // leg whose ORIGINAL request is still IN FLIGHT (verifier F4,
            // dated 2026-07-15: the pre-fix "Err OR still pending" read
            // fired a duplicate concurrent same-leg request while the
            // original was mid-flight — double request volume against a
            // slow broker for zero data gain). A still-in-flight leg is
            // SKIPPED (await-or-skip; its completion still lands
            // first-write-wins, and an Err completion after the verdict
            // is terminal on its 1st attempt — see `groww_leg_attempts`).
            // Failures re-fetch sequentially: chains first, then spots;
            // successes never re-fetched.
            let failed_chains: Vec<usize> = (0..ChainUnderlying::COUNT)
                .filter(|i| !cycle.groww_leg_ok[*i] && !cycle.groww_leg_inflight[*i])
                .collect();
            let failed_spots: Vec<usize> = (0..SpotTarget::ALL.len())
                .filter(|k| {
                    let leg = k + ChainUnderlying::COUNT;
                    !cycle.groww_leg_ok[leg] && !cycle.groww_leg_inflight[leg]
                })
                .collect();
            if failed_chains.is_empty() && failed_spots.is_empty() {
                return;
            }
            cycle.groww_fallback_launched = true;
            cycle.groww.flags.groww_fallback = true;
            // ExpiryResolver seam (2026-07-15): the fallback task runs
            // detached, so the expiry stamps + the coalesced
            // `expiry_unresolved` flag are resolved HERE at launch time
            // and moved into the task.
            let failed_chain_reqs: Vec<(usize, Option<u32>)> = failed_chains
                .iter()
                .map(|&i| {
                    let expiry_yyyymmdd = deps.expiry_resolver.resolved_expiry(
                        Feed::Groww,
                        ChainUnderlying::ALL[i],
                        clock.ist_date(),
                    );
                    if expiry_yyyymmdd.is_none() {
                        cycle.groww.flags.expiry_unresolved = true;
                    }
                    (i, expiry_yyyymmdd)
                })
                .collect();
            for i in &failed_chains {
                metrics::counter!("tv_cadence_groww_fallback_total", "leg" => "chain").increment(1);
                debug!(underlying_idx = i, "cadence: groww chain fallback queued");
                cycle.groww.inflight = cycle.groww.inflight.saturating_add(1);
                cycle.groww_leg_inflight[*i] = true;
            }
            for k in &failed_spots {
                metrics::counter!("tv_cadence_groww_fallback_total", "leg" => "spot").increment(1);
                debug!(target_idx = k, "cadence: groww spot fallback queued");
                cycle.groww.inflight = cycle.groww.inflight.saturating_add(1);
                cycle.groww_leg_inflight[*k + ChainUnderlying::COUNT] = true;
            }
            let exec = Arc::clone(&deps.groww_executor);
            let fallback_tx = tx.clone();
            let timeout_ms = deps.config.groww_request_timeout_ms;
            let cycle_minute = slots.cycle_minute_ist;
            let task_clock = Arc::clone(clock);
            // Fire-and-forget by design: the task is bounded by the
            // per-request timeouts and dies with the channel.
            drop(tokio::spawn(async move {
                // Sequential: failed CHAINS first, then failed SPOTS
                // (second-1 / second-2 windows, design §1). Each leg's
                // deadline is stamped AT ITS OWN DISPATCH instant —
                // `groww_request_timeout_ms` bounds each individual
                // request (design §0); a shared verdict-time base would
                // hand legs 2..N already-consumed deadlines.
                for (i, expiry_yyyymmdd) in failed_chain_reqs {
                    let req = ChainFetchRequest {
                        feed: Feed::Groww,
                        underlying: ChainUnderlying::ALL[i],
                        expiry_yyyymmdd,
                        cycle_minute_ist: cycle_minute,
                        deadline_epoch_ms: task_clock.epoch_ms().saturating_add(timeout_ms),
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
                        deadline_epoch_ms: task_clock.epoch_ms().saturating_add(timeout_ms),
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
            finalize_lane_at_cutoff(clock.as_ref(), slots, groww, dhan, latch, deps.dry_run);
        }
        CycleAction::DhanCutoff => {
            let CycleState { dhan, groww, .. } = cycle;
            finalize_lane_at_cutoff(clock.as_ref(), slots, dhan, groww, latch, deps.dry_run);
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

/// Handle one fetch completion: record, count, retry-policy (incl. the
/// L3 DEFERRED Groww per-leg fallback, 2026-07-15), and attempt
/// event-driven finalize for BOTH lanes (cross-fill runs inside).
// APPROVED: the completion dispatcher threads the whole cycle state + the runner deps — one private fn with one call site.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn handle_completion<C, D, G>(
    clock: &Arc<C>,
    deps: &CadenceRunnerDeps<D, G>,
    slots: &CycleSlots,
    completion: Completion,
    cycle: &mut CycleState,
    latch: &mut DecisionLatch,
    tx: &mpsc::Sender<Completion>,
) where
    C: CadenceClock,
    D: CadenceExecutor + 'static,
    G: CadenceExecutor + 'static,
{
    let cfg = &deps.config;
    let dry_run = deps.dry_run;
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

    // CONC-NEW-1: completion-time re-read of the runtime lane toggles —
    // a disable lands here BEFORE the retry/deferred-fallback paths can
    // dispatch a fresh request from a disabled lane (the resolved flag
    // this sets gates them), and before finalize can emit its decision.
    observe_runtime_lane_toggles(deps, cycle);

    {
        let lane: &mut LaneRun = match lane_feed {
            Feed::Dhan => &mut cycle.dhan,
            Feed::Groww => &mut cycle.groww,
        };
        lane.inflight = lane.inflight.saturating_sub(1);
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
                if lane_feed == Feed::Groww {
                    cycle.groww_leg_attempts[underlying_idx] =
                        cycle.groww_leg_attempts[underlying_idx].saturating_add(1);
                    cycle.groww_leg_inflight[underlying_idx] = false;
                }
                match result {
                    Ok(ok) => {
                        if lane_feed == Feed::Groww {
                            cycle.groww_leg_ok[underlying_idx] = true;
                        }
                        lane.asm.record_chain(
                            underlying,
                            ChainCell {
                                provenance: ChainProvenance::OwnFetch,
                                source_feed: lane_feed,
                                published_to_registry: ok.published_to_registry,
                                fetched_at_ms: now_wall,
                                minute_ist: slots.cycle_minute_ist,
                                embedded_spot: ok.underlying_spot,
                            },
                        );
                    }
                    Err(err) => {
                        if matches!(err, CadenceFetchError::Empty) {
                            // Chain 200-empty: its own coalesced stage
                            // (never conflated with a transport-class
                            // fetch_failed); does NOT arm the ladder.
                            lane.flags.chain_empty = true;
                        }
                        record_failure(lane, &err);
                        if lane_feed == Feed::Groww
                            && matches!(err, CadenceFetchError::RateLimited { .. })
                        {
                            // Any RateLimited Groww leg (chain OR spot)
                            // marks the fallback-shape cycle dirty
                            // (2026-07-15). Dhan CHAIN 429s deliberately
                            // do NOT feed the SPOT concurrency ladder —
                            // the chain gates are unchanged.
                            cycle.groww_dirty = true;
                        }
                        let mut retry_scheduled = false;
                        if lane_feed == Feed::Dhan {
                            let earliest = slots
                                .dhan_chain_retry_slots_ms
                                .get(cycle.next_chain_retry_slot)
                                .copied();
                            // The cutoff-landing admission tests the
                            // ACTUAL insertion instant (verifier F9,
                            // dated 2026-07-15): a grid slot already in
                            // the past clamps forward to `now`, so the
                            // pre-fix grid-instant test could admit a
                            // retry whose REAL fire (+ latency allowance)
                            // lands past the lane cutoff — a structurally
                            // late (discarded) response.
                            if let Some(retry_at) = earliest {
                                let retry_fire = retry_at.max(now_wall);
                                if may_retry_in_cycle(
                                    &err,
                                    cycle.chain_retries_used[underlying_idx],
                                    cfg.in_cycle_retry_max,
                                    retry_fire,
                                    CADENCE_RETRY_LATENCY_ALLOWANCE_MS,
                                    slots.dhan_cutoff_ms,
                                ) {
                                    cycle.chain_retries_used[underlying_idx] += 1;
                                    cycle.next_chain_retry_slot += 1;
                                    retry_scheduled = true;
                                    insert_event(
                                        &mut cycle.events,
                                        retry_fire,
                                        CycleAction::DhanChain {
                                            underlying_idx,
                                            nominal: false,
                                        },
                                    );
                                }
                            }
                        }
                        // L3 (2026-07-15): the DEFERRED per-leg fallback
                        // — a leg SKIPPED in flight at the verdict (F4)
                        // whose original request completes Err AFTER the
                        // verdict has no later verdict to refetch it. Its
                        // ONE fallback attempt dispatches IMMEDIATELY at
                        // this completion when it can still land inside
                        // the lane cutoff (mirrors the verdict fallback:
                        // any Err class, expiry resolved at dispatch,
                        // first-write-wins on completion; never a
                        // concurrent duplicate — the original already
                        // completed).
                        let mut deferred_fallback = false;
                        if lane_feed == Feed::Groww
                            && cycle.groww_verdict_passed
                            && cycle.groww_leg_attempts[underlying_idx] == 1
                            && !lane.resolved
                            && now_wall.saturating_add(CADENCE_RETRY_LATENCY_ALLOWANCE_MS)
                                <= slots.groww_cutoff_ms
                        {
                            deferred_fallback = true;
                            lane.flags.groww_fallback = true;
                            metrics::counter!(
                                "tv_cadence_groww_fallback_total",
                                "leg" => "chain"
                            )
                            .increment(1);
                            debug!(
                                underlying_idx,
                                "cadence: groww chain DEFERRED fallback dispatched (L3)"
                            );
                            let expiry_yyyymmdd = deps.expiry_resolver.resolved_expiry(
                                Feed::Groww,
                                underlying,
                                clock.ist_date(),
                            );
                            if expiry_yyyymmdd.is_none() {
                                lane.flags.expiry_unresolved = true;
                            }
                            let req = ChainFetchRequest {
                                feed: Feed::Groww,
                                underlying,
                                expiry_yyyymmdd,
                                cycle_minute_ist: slots.cycle_minute_ist,
                                deadline_epoch_ms: clock
                                    .epoch_ms()
                                    .saturating_add(deps.config.groww_request_timeout_ms),
                            };
                            lane.inflight = lane.inflight.saturating_add(1);
                            cycle.groww_leg_inflight[underlying_idx] = true;
                            spawn_chain_fetch(
                                Arc::clone(&deps.groww_executor),
                                tx.clone(),
                                req,
                                underlying_idx,
                                deps.config.groww_request_timeout_ms,
                            );
                        }
                        // `fetch_failed` = the rule-file definition: a
                        // non-Empty failure AFTER the retry budget (Dhan:
                        // no retry admitted; Groww: the fallback attempt
                        // itself failed, or no fallback could land — L3:
                        // an in-flight-skipped leg whose deferred fallback
                        // dispatched is NOT terminal on its 1st attempt)
                        // with the cell still missing. QueueDelay is
                        // stage-tagged distinctly (its own coalesced
                        // stage; F1(iii)) — never conflated with the
                        // transport-class fetch_failed.
                        let terminal = match lane_feed {
                            Feed::Dhan => !retry_scheduled,
                            Feed::Groww => {
                                !deferred_fallback
                                    && (cycle.groww_leg_attempts[underlying_idx] >= 2
                                        || cycle.groww_verdict_passed)
                            }
                        };
                        if terminal
                            && !matches!(
                                err,
                                CadenceFetchError::Empty | CadenceFetchError::QueueDelay
                            )
                            && lane.asm.chain(underlying).is_none()
                        {
                            lane.flags.fetch_failed = true;
                        }
                    }
                }
            }
            CompletionKind::Spot { target_idx, result } => {
                let target = SpotTarget::ALL[target_idx];
                if lane_feed == Feed::Groww {
                    let leg = target_idx + ChainUnderlying::COUNT;
                    cycle.groww_leg_attempts[leg] = cycle.groww_leg_attempts[leg].saturating_add(1);
                    cycle.groww_leg_inflight[leg] = false;
                }
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
                        if matches!(err, CadenceFetchError::Empty) {
                            // 200-empty: coalesced spot_empty stage
                            // (either lane); does NOT arm the ladder
                            // (Assumed, design §0).
                            lane.flags.spot_empty = true;
                        }
                        record_failure(lane, &err);
                        if matches!(err, CadenceFetchError::RateLimited { .. }) {
                            // Feed the streak ladders (2026-07-15): a
                            // RateLimited SPOT outcome marks the Dhan
                            // spot-concurrency cycle dirty; any
                            // RateLimited Groww leg marks the
                            // fallback-shape cycle dirty.
                            match lane_feed {
                                Feed::Dhan => cycle.dhan_spot_dirty = true,
                                Feed::Groww => cycle.groww_dirty = true,
                            }
                        }
                        // The retry is APPENDED at the next free
                        // rolling-window instant AFTER the last group
                        // anchor (design §1 "spot retries appended",
                        // 2026-07-15 gate change) — an appended retry can
                        // never contend a nominal group's window budget.
                        let mut retry_scheduled = false;
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
                                    retry_target.saturating_add(CADENCE_SPOT_WINDOW_MS);
                                retry_scheduled = true;
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
                        // L3 (2026-07-15): the DEFERRED per-leg fallback
                        // for an in-flight-skipped SPOT leg — see the
                        // chain arm above.
                        let mut deferred_fallback = false;
                        if lane_feed == Feed::Groww
                            && cycle.groww_verdict_passed
                            && cycle.groww_leg_attempts[target_idx + ChainUnderlying::COUNT] == 1
                            && !lane.resolved
                            && now_wall.saturating_add(CADENCE_RETRY_LATENCY_ALLOWANCE_MS)
                                <= slots.groww_cutoff_ms
                        {
                            deferred_fallback = true;
                            lane.flags.groww_fallback = true;
                            metrics::counter!(
                                "tv_cadence_groww_fallback_total",
                                "leg" => "spot"
                            )
                            .increment(1);
                            debug!(
                                target_idx,
                                "cadence: groww spot DEFERRED fallback dispatched (L3)"
                            );
                            let req = SpotFetchRequest {
                                feed: Feed::Groww,
                                target,
                                cycle_minute_ist: slots.cycle_minute_ist,
                                deadline_epoch_ms: clock
                                    .epoch_ms()
                                    .saturating_add(deps.config.groww_request_timeout_ms),
                            };
                            lane.inflight = lane.inflight.saturating_add(1);
                            cycle.groww_leg_inflight[target_idx + ChainUnderlying::COUNT] = true;
                            spawn_spot_fetch(
                                Arc::clone(&deps.groww_executor),
                                tx.clone(),
                                req,
                                target_idx,
                                deps.config.groww_request_timeout_ms,
                            );
                        }
                        let terminal = match lane_feed {
                            Feed::Dhan => !retry_scheduled,
                            Feed::Groww => {
                                !deferred_fallback
                                    && (cycle.groww_leg_attempts
                                        [target_idx + ChainUnderlying::COUNT]
                                        >= 2
                                        || cycle.groww_verdict_passed)
                            }
                        };
                        let cell_missing = match target.chain_underlying() {
                            Some(u) => lane.asm.spot(u).is_none(),
                            None => lane.asm.vix_spot().is_none(),
                        };
                        if terminal
                            && !matches!(
                                err,
                                CadenceFetchError::Empty | CadenceFetchError::QueueDelay
                            )
                            && cell_missing
                        {
                            lane.flags.fetch_failed = true;
                        }
                    }
                }
            }
        }
    }
    // Event-driven finalize: a decision fires the INSTANT a lane's
    // predicate completes ON OWN DATA; the fallback rungs (cross-fill +
    // chain-embedded) are admitted only once the lane's OWN path is
    // exhausted (design §5 resolution order — own fetch first, fallback
    // never preempts a still-scheduled own fire).
    let dhan_exhausted = lane_own_path_exhausted(Feed::Dhan, cycle);
    let groww_exhausted = lane_own_path_exhausted(Feed::Groww, cycle);
    let CycleState { dhan, groww, .. } = cycle;
    finalize_if_complete(
        clock.as_ref(),
        slots,
        dhan,
        groww,
        latch,
        dhan_exhausted,
        dry_run,
    );
    finalize_if_complete(
        clock.as_ref(),
        slots,
        groww,
        dhan,
        latch,
        groww_exhausted,
        dry_run,
    );
}

/// Is the lane's OWN fetch path exhausted for this cycle? TRUE when the
/// lane has no in-flight fetch AND no remaining scheduled own event
/// (primaries, retries, the Groww waves/verdict — cutoffs are not own
/// work). Only then may the fallback rungs run before the cutoff.
fn lane_own_path_exhausted(feed: Feed, cycle: &CycleState) -> bool {
    let lane = match feed {
        Feed::Dhan => &cycle.dhan,
        Feed::Groww => &cycle.groww,
    };
    if lane.inflight > 0 {
        return false;
    }
    !cycle.events.iter().any(|(_, action)| match feed {
        Feed::Dhan => matches!(
            action,
            CycleAction::DhanChain { .. } | CycleAction::DhanSpot { .. }
        ),
        Feed::Groww => matches!(
            action,
            CycleAction::GrowwWave { .. } | CycleAction::GrowwVerdict
        ),
    })
}

/// Count + classify a fetch failure on its lane (`fetch_failed` is the
/// CALLER's terminal-classification duty — see `handle_completion`).
fn record_failure(lane: &mut LaneRun, err: &CadenceFetchError) {
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
            "CADENCE-01: broker 429 despite the gates — arms the shape \
             ladder; ONE bounded in-cycle retry through the gates \
             (gate-bug / co-tenant signal)"
        );
    }
    if matches!(err, CadenceFetchError::QueueDelay) {
        // SELF-INFLICTED limiter pacing (F1(iii), 2026-07-15): its own
        // coalesced stage; failure_arms_ladder already refuses it, so
        // the arming check below is structurally false for it too.
        lane.flags.queue_delay = true;
    }
    if lane.asm.feed == Feed::Dhan && failure_arms_ladder(err) {
        lane.arming_failure = true;
    }
}

/// The finalize core: decide the instant the predicate completes on OWN
/// data; run the cross-fill + chain-embedded fallback rungs ONLY when
/// `own_path_exhausted` (or from the cutoff's last-chance call) — the
/// design §5 resolution ORDER: a fallback never preempts a lane's own
/// still-scheduled fires. NEVER a late decision: past the lane cutoff
/// this returns untouched and the cutoff event owns resolution
/// (honest-skip, design §5).
// APPROVED: the finalize core threads the F10 dry-run mode — private fn, two call sites.
#[allow(clippy::too_many_arguments)]
fn finalize_if_complete<C: CadenceClock>(
    clock: &C,
    slots: &CycleSlots,
    lane: &mut LaneRun,
    other: &LaneRun,
    latch: &mut DecisionLatch,
    own_path_exhausted: bool,
    dry_run: bool,
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
    if !may_decide_at_completion(now_wall, cutoff) {
        // Past the cutoff there is NO decide path — the queued cutoff
        // event emits the honest skip ("never a late decision"). A
        // completion processed after the cutoff instant (unbiased select
        // race / stalled runner) must not produce a late Decided. The
        // comparison is the pure, unit-pinned
        // `decision::may_decide_at_completion` and this call site is
        // source-scan-ratcheted (TRH-R2-1, 2026-07-15) — deleting or
        // inverting the guard fails the build.
        return;
    }
    if !lane.asm.is_data_complete() {
        if !own_path_exhausted {
            // The lane still has own fires scheduled or in flight — the
            // fallback rungs must not preempt them (a healthy dual-lane
            // cycle would otherwise cross-fill Dhan from the Groww burst
            // at ~T+0.3 and suppress every Dhan own fire).
            return;
        }
        // Rung 2: cross-source fill from the other lane's same-cycle data
        // (freshness-checked against the plain base floor T − 5000 —
        // every fire is post-close since the 2026-07-16 shape change, so
        // the retired lender-aware widening is unnecessary; valid up to
        // AND INCLUDING the cutoff).
        let floor = cross_fill_freshness_floor_ms(slots);
        let (spots, chains) = lane
            .asm
            .cross_fill_from(&other.asm, floor, now_wall, cutoff);
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
    decide_lane(clock, slots, lane, latch, dry_run);
}

/// IST-epoch nanoseconds "now" (the `chain_snapshot` registry's time
/// domain) derived from the injected clock's UTC epoch milliseconds.
fn ist_epoch_nanos<C: CadenceClock>(clock: &C) -> i64 {
    clock
        .epoch_ms()
        .saturating_add(i64::from(IST_UTC_OFFSET_SECONDS).saturating_mul(1_000))
        .saturating_mul(1_000_000)
}

/// Emit the lane's decision (Decided / DecidedDegraded / Skipped
/// AllUnknown), exactly once via the latch.
fn decide_lane<C: CadenceClock>(
    clock: &C,
    slots: &CycleSlots,
    lane: &mut LaneRun,
    latch: &mut DecisionLatch,
    dry_run: bool,
) {
    let now_wall = clock.ist_ms_of_day();
    let now_ist_nanos = ist_epoch_nanos(clock);
    let feed = lane.asm.feed;
    let mut folds = [MoneynessFold::default(); ChainUnderlying::COUNT];
    let mut provenance: [Option<SpotProvenance>; ChainUnderlying::COUNT] =
        [None; ChainUnderlying::COUNT];
    for u in ChainUnderlying::ALL {
        let prov = lane.asm.spot(*u).map(|s| s.provenance);
        // CHAIN-ROW anchor order (R5, 2026-07-16): the chain's OWN
        // embedded underlying spot FIRST (same-response coherence),
        // the resolved spot cell as the fallback, Unknown last — the
        // OwnFetch spot serves the SPOT SERIES, not chain moneyness.
        let (spot_paise, atm_paise) =
            chain_moneyness_anchor(*u, lane.asm.chain(*u), lane.asm.spot(*u));
        // GUARDED fold over the resolved cell: reads the cell's SOURCE
        // feed's registry slot (the lender's for a cross-filled chain),
        // refuses an unconfirmed publish and a stale / wrong-minute /
        // sentinel snapshot — a refusal folds to 0 rows (all_unknown,
        // SURFACED), never a silent stale-row classification (design §6).
        let fold = lane
            .asm
            .chain(*u)
            .map_or_else(MoneynessFold::default, |cell| {
                fold_chain_cell_moneyness(
                    cell,
                    *u,
                    lane.asm.cycle_minute_ist,
                    now_ist_nanos,
                    spot_paise,
                    atm_paise,
                )
            });
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
        // Should-never exactly-once breach — refused LOUDLY, never a
        // panic path (F10, 2026-07-15: the pre-fix debug_assert!(false)
        // aborted unwind test builds and was silent in release).
        metrics::counter!("tv_cadence_double_latch_total", "lane" => feed.as_str()).increment(1);
        error!(
            code = ErrorCode::Cadence03SchedulerDegraded.code_str(),
            stage = "double_latch",
            lane = feed.as_str(),
            cycle_minute_ist = lane.asm.cycle_minute_ist,
            "CADENCE-03: decision double-latch attempt refused (exactly-\
             once guard held; should-never scheduler-logic signal)"
        );
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
    emit_decision(
        &DecisionSnapshot {
            lane: feed,
            cycle_minute_ist: lane.asm.cycle_minute_ist,
            outcome,
            vix_missing: lane.asm.vix_missing(),
            post_close: slots.post_close,
            latency_ms: now_wall.saturating_sub(slots.boundary_ms),
            moneyness: folds,
            spot_provenance: provenance,
        },
        dry_run,
    );
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
    dry_run: bool,
) {
    if !lane.enabled || lane.resolved {
        return;
    }
    // Last chance: the cross-fill window is valid up to AND INCLUDING
    // the cutoff instant (a cutoff event popping even 1ms late finds the
    // finalize guard refusing — fail-closed, the skip below owns it).
    finalize_if_complete(clock, slots, lane, other, latch, true, dry_run);
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
    emit_decision(
        &DecisionSnapshot {
            lane: lane.asm.feed,
            cycle_minute_ist: lane.asm.cycle_minute_ist,
            outcome: DecisionOutcome::Skipped(reason),
            vix_missing: lane.asm.vix_missing(),
            post_close: slots.post_close,
            latency_ms: now_wall.saturating_sub(slots.boundary_ms),
            moneyness: [MoneynessFold::default(); ChainUnderlying::COUNT],
            spot_provenance: [None; ChainUnderlying::COUNT],
        },
        dry_run,
    );
    lane.resolved = true;
}
