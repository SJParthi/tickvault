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
use tickvault_common::constants::{
    CADENCE_DECISION_DEADLINE_MS, CADENCE_NATIVE_RETRY_OFFSETS_MS, CADENCE_SPOT_WINDOW_MS,
    IST_UTC_OFFSET_SECONDS,
};
use tickvault_common::error_code::ErrorCode;
use tickvault_common::feed::Feed;
use tickvault_common::trading_calendar::{TradingCalendar, ist_offset};
use tokio::sync::{Notify, mpsc};
use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};

use super::assembly::{
    ChainCell, ChainProvenance, LaneAssembly, MoneynessFold, SpotProvenance,
    chain_moneyness_anchor, fold_chain_cell_moneyness, spots_diverge_paise,
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
    SPOT_CONCURRENCY_MAX_STEP, StreakLadder, StreakShift, failure_arms_ladder, may_retry_in_cycle,
    min_spot_step_for_cap,
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
pub struct CadenceRunnerDeps<D> {
    /// The validated `[cadence]` config.
    pub config: CadenceConfig,
    /// Trading-day calendar.
    pub calendar: Arc<TradingCalendar>,
    /// The Dhan lane executor (the dry-run logger in this PR).
    pub dhan_executor: Arc<D>,
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
    // 2026-08-21: the `notifier` field was REMOVED here. Its only consumer
    // was the expiry cross-broker DISAGREEMENT page, and a disagreement
    // needs two brokers to disagree; with one, the latch that fed it can
    // never be set. Keeping the field would have had boot hand the runner a
    // Telegram sink that nothing could ever send through -- a wired sensor
    // with no read-out, which reads greener than no sink at all.
    /// Graceful-shutdown signal (`notify_waiters` at teardown).
    pub shutdown: Arc<Notify>,
}

impl<D> Clone for CadenceRunnerDeps<D> {
    fn clone(&self) -> Self {
        Self {
            config: self.config.clone(),
            calendar: Arc::clone(&self.calendar),
            dhan_executor: Arc::clone(&self.dhan_executor),
            expiry_resolver: Arc::clone(&self.expiry_resolver),
            expiry_store: self.expiry_store.as_ref().map(Arc::clone),
            gates: Arc::clone(&self.gates),
            dry_run: self.dry_run,
            dhan_enabled: Arc::clone(&self.dhan_enabled),
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
pub fn spawn_supervised_cadence_runner<D>(deps: CadenceRunnerDeps<D>) -> JoinHandle<()>
where
    D: CadenceExecutor + 'static,
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
pub async fn run_cadence_loop<C, D>(clock: Arc<C>, deps: CadenceRunnerDeps<D>) -> LoopExit
where
    C: CadenceClock,
    D: CadenceExecutor + 'static,
{
    // Seed the cadence loop's own failure series at zero, once, as the loop
    // starts — so an absent reading can never pass for a clean run.
    //
    // A live sweep on 2026-08-29 found all three had NEVER published a
    // datapoint despite being EMF-selected. A skipped boundary and a runner
    // respawn are both born at the failure, and the CloudWatch agent drops the
    // first sample of a series it has never seen, so the first occurrence of
    // each was structurally invisible. Seeded here, in the loop itself, rather
    // than at boot: these describe the cadence runner, and a confident zero
    // for a runner that never started would be a worse lie than silence.
    metrics::counter!("tv_cadence_boundary_skipped_total").increment(0);
    metrics::counter!("tv_cadence_gate_denials_total").increment(0);
    for reason in ["clean_exit", "panic", "cancelled", "unknown"] {
        metrics::counter!("tv_cadence_runner_respawn_total", "reason" => reason).increment(0);
    }
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
    let mut latch = DecisionLatch::new();
    let mut last_boundary: Option<u32> = None;
    // M9 (audit 2026-07-20): once-per-day latch for the session-tail
    // accounting in the no-joinable-boundary arm.
    let mut final_tail_accounted = false;
    let mut current_date = clock.ist_date();
    let mut exhausted_episode = false;
    let mut lanes_parked = false;
    metrics::gauge!("tv_cadence_dhan_shape_step").set(f64::from(dhan_shape_ladder.step));
    metrics::gauge!("tv_cadence_spot_concurrency_step").set(f64::from(spot_ladder.step));

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
            exhausted_episode = false;
            last_boundary = None;
            final_tail_accounted = false;
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
        }

        // BOTH lanes disabled ⇒ PARK level-triggered WITHOUT consuming
        // boundaries: an instant-resolving all-disabled cycle would
        // otherwise burn every remaining boundary of the day in one tick,
        // silently killing the cadence until the next IST day (a transient
        // dual-disable via /api/feeds must recover at the next real
        // minute). Missed boundaries are counted LOUD on resume by the
        // boundary_skipped check below.
        let dhan_on = deps.dhan_enabled.load(Ordering::Acquire);
        if !dhan_on {
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
            // M9 (audit 2026-07-20, Dim D F1): a stall/skew that overruns
            // PAST session end lands here with the day's TAIL boundaries
            // (incl. the 15:29 decision minute) consumed by NOTHING — the
            // in-session boundary_skipped arm below never sees them
            // because no further joinable boundary exists. Account them
            // LOUDLY, once per IST day (latched; a fresh post-close boot
            // with `last_boundary = None` never claims a tail it did not
            // own — that class is the boot-liveness alarm's).
            if is_trading
                && !final_tail_accounted
                && let Some(missed) = unaccounted_session_tail(last_boundary)
            {
                final_tail_accounted = true;
                metrics::counter!("tv_cadence_boundary_skipped_total").increment(u64::from(missed));
                error!(
                    code = ErrorCode::Cadence03SchedulerDegraded.code_str(),
                    stage = "final_boundary_missed",
                    missed,
                    from_boundary = ?last_boundary,
                    "CADENCE-03: session-tail cycle boundaries dropped past \
                     15:30 IST un-fired (stall/overrun past session end) — \
                     the final decisions of the day were lost"
                );
            }
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

        let slots = build_cycle_slots(boundary, dhan_shape_ladder.step, spot_ladder.step, &cfg);
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
        let (dhan_dirty, dhan_spot_dirty) = match outcome {
            CycleRun::Shutdown => return LoopExit::Shutdown,
            // The IST calendar date changed mid-cycle (suspend across
            // midnight) — the cycle was dropped with no partial emit; the
            // loop top re-reads the date and resets the day state.
            CycleRun::Abandoned => continue,
            CycleRun::Verdict {
                dhan_dirty,
                dhan_spot_dirty,
                ..
            } => (dhan_dirty, dhan_spot_dirty),
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
async fn run_expiry_resolution_loop<C, D>(
    clock: Arc<C>,
    deps: CadenceRunnerDeps<D>,
    store: Arc<DayLockedExpiryStore>,
) where
    C: CadenceClock,
    D: CadenceExecutor + 'static,
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
                )
                .await;
            }
            // E4 + R3: fold this iteration's wave into the per-pair
            // REAL-failed counters — only pairs whose fetch was actually
            // dispatched AND that stayed unresolved advance.
            for feed in [Feed::Dhan] {
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
            for (feed, enabled) in [(Feed::Dhan, dhan_on)] {
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
    },
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
    /// The Dhan lane staleness cutoff.
    DhanCutoff,
    /// The boundary+deadline native-retry chain deadline slot (item 3).
    NativeDeadline,
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
    chain_embedded_spot: bool,
    moneyness_unknown: bool,
    /// ADVISORY (H3/H2-partial, audit 2026-07-20): a chain body's embedded
    /// underlying spot diverged from the lane's resolved spot cell beyond
    /// the 0.5% coherence band — vendor-stale chain / wrong-instrument
    /// proxy (chain bodies carry no vendor timestamp or echo to check
    /// directly). Never decision-blocking, never arming. 2026-07-20
    /// (adversarial review): EXCLUDED from `any()`/`stages()` — advisory
    /// info-level + counter only, never a CADENCE-01 degrade stage.
    chain_spot_divergence: bool,
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
    /// Resolution provenance token for the cross-fill audit seam
    /// (item 3): set IMMEDIATELY before `resolved = true`.
    resolution: Option<&'static str>,
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
    /// Native spot retry rungs consumed this cycle (item 2 phase B1).
    late_retry_attempts: u32,
    /// Latched on a 429 seen while the native retry ladder is active -
    /// aborts the remaining native rungs for this cycle (budget-spent).
    retry_rate_limited: bool,
}

impl LaneRun {
    fn new(feed: Feed, enabled: bool, slots: &CycleSlots) -> Self {
        Self {
            enabled,
            state: CadenceState::Idle,
            asm: LaneAssembly::new(feed, slots.cycle_minute_ist, slots.boundary_ms),
            resolved: false,
            resolution: None,
            flags: DegradeFlags::default(),
            arming_failure: false,
            inflight: 0,
            late_retry_attempts: 0,
            retry_rate_limited: false,
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
    native_retry_enabled: bool,
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
    if native_retry_enabled {
        events.push((
            slots.boundary_ms + CADENCE_DECISION_DEADLINE_MS,
            CycleAction::NativeDeadline,
        ));
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
async fn run_cycle<C, D>(
    clock: &Arc<C>,
    deps: &CadenceRunnerDeps<D>,
    gates: &Arc<DhanGates>,
    slots: &CycleSlots,
    latch: &mut DecisionLatch,
    demote_nominal: bool,
    mut shutdown_fut: std::pin::Pin<&mut tokio::sync::futures::Notified<'_>>,
) -> CycleRun
where
    C: CadenceClock,
    D: CadenceExecutor + 'static,
{
    let dhan_enabled = deps.dhan_enabled.load(Ordering::Acquire);
    let cycle_date = clock.ist_date();

    let mut cycle = CycleState {
        dhan: LaneRun::new(Feed::Dhan, dhan_enabled, slots),
        events: Vec::new(),
        chain_retries_used: [0; 3],
        spot_retries_used: [0; 4],
        next_chain_retry_slot: 0,
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
        dispatched_any: false,
    };
    // Anchor FSM arming (level-triggered per cycle per lane).
    arm_lane(&mut cycle.dhan);
    if cycle.dhan.resolved {
        return CycleRun::Verdict {
            dhan_dirty: false,
            dhan_spot_dirty: false,
        };
    }

    cycle.events = build_cycle_events(slots, cycle.dhan.enabled, deps.config.native_retry_enabled);

    let (tx, mut rx) = mpsc::channel::<Completion>(CADENCE_COMPLETION_CHANNEL_CAPACITY);

    loop {
        if cycle.dhan.resolved && cycle.events.is_empty() {
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
                    if dhan_now != cycle.dhan.enabled {
                        info!(
                            dhan_enabled = dhan_now,
                            "cadence: runtime lane toggle observed before the \
                             cycle's first fire — cycle re-armed from the \
                             fresh flags"
                        );
                        cycle.dhan = LaneRun::new(Feed::Dhan, dhan_now, slots);
                        arm_lane(&mut cycle.dhan);
                        cycle.events = build_cycle_events(
                            slots,
                            dhan_now,
                            deps.config.native_retry_enabled,
                        );
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
    for lane in [&cycle.dhan] {
        if lane.enabled && lane.flags.any() {
            if deps.dry_run && !lane.flags.fetch_failed && !lane.flags.rate_limited {
                info!(
                    dry_run = true,
                    stage = %lane.flags.stages(),
                    lane = lane.asm.feed.as_str(),
                    cycle_minute_ist = lane.asm.cycle_minute_ist,
                    attempts = lane.late_retry_attempts,
                    resolution = lane.resolution.unwrap_or("none"),
                    "cadence lane degraded under DRY-RUN executors \
                     (expected shape — F10 demotion)"
                );
            } else {
                error!(
                    code = ErrorCode::Cadence01LaneDegraded.code_str(),
                    stage = %lane.flags.stages(),
                    lane = lane.asm.feed.as_str(),
                    cycle_minute_ist = lane.asm.cycle_minute_ist,
                    attempts = lane.late_retry_attempts,
                    resolution = lane.resolution.unwrap_or("none"),
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
    CycleRun::Verdict {
        dhan_dirty: cycle.dhan.enabled && cycle.dhan.arming_failure,
        dhan_spot_dirty: cycle.dhan_spot_dirty,
    }
}

/// The whole per-cycle mutable state, threaded as ONE unit (borrow
/// hygiene for the action/completion dispatchers).
struct CycleState {
    dhan: LaneRun,
    events: Vec<(i64, CycleAction)>,
    chain_retries_used: [u32; 3],
    spot_retries_used: [u32; 4],
    next_chain_retry_slot: usize,
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
        // SEAM(#1688): resolution provenance recorded exactly once per site.
        lane.resolution = Some(resolution_token(lane.late_retry_attempts));
        lane.resolved = true;
        debug_assert!(lane.resolution.is_some(), "resolution set before resolved");
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
fn observe_runtime_lane_toggles<D>(deps: &CadenceRunnerDeps<D>, cycle: &mut CycleState)
where
    D: CadenceExecutor + 'static,
{
    if cycle.dhan.enabled && !deps.dhan_enabled.load(Ordering::Acquire) {
        drop_lane_runtime_disabled(&mut cycle.dhan);
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
    // SEAM(#1688): resolution provenance recorded exactly once per site.
    lane.resolution = Some(resolution_token(lane.late_retry_attempts));
    lane.resolved = true;
    debug_assert!(lane.resolution.is_some(), "resolution set before resolved");
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
fn handle_action<C, D>(
    clock: &Arc<C>,
    deps: &CadenceRunnerDeps<D>,
    gates: &Arc<DhanGates>,
    slots: &CycleSlots,
    action: CycleAction,
    cycle: &mut CycleState,
    tx: &mpsc::Sender<Completion>,
    latch: &mut DecisionLatch,
) where
    C: CadenceClock,
    D: CadenceExecutor + 'static,
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
                    // H4 (native retry): a gate-denied NATIVE rung is skipped
                    // silently - counted, never re-queued, never escalated.
                    // Native rungs are scheduled nominal=false with
                    // late_retry_attempts > 0 (incremented at schedule time),
                    // so nominal dispatches and pure-legacy retries keep the
                    // byte-equivalent defer_action path below.
                    if !nominal
                        && deps.config.native_retry_enabled
                        && cycle.dhan.late_retry_attempts > 0
                    {
                        metrics::counter!(
                            "tv_cadence_native_retry_total",
                            "lane" => Feed::Dhan.as_str(),
                            "outcome" => "gate_busy_skip"
                        )
                        .increment(1);
                        return;
                    }
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
        CycleAction::NativeDeadline => {
            // ITEM 3: ONE gate-paced chain deadline slot per underlying at
            // boundary + CADENCE_DECISION_DEADLINE_MS - history-repair only
            // (results ride the normal completion flow; never deferred).
            if cycle.dhan.enabled && !cycle.dhan.resolved {
                cycle.dhan.late_retry_attempts = cycle.dhan.late_retry_attempts.saturating_add(1);
                for (underlying_idx, underlying) in ChainUnderlying::ALL.iter().copied().enumerate()
                {
                    let expiry_yyyymmdd = deps.expiry_resolver.resolved_expiry(
                        Feed::Dhan,
                        underlying,
                        clock.ist_date(),
                    );
                    match gates.try_acquire_chain(underlying, expiry_yyyymmdd, now_mono) {
                        GateVerdict::Acquired => {
                            if cycle.dhan.state == CadenceState::Armed {
                                cycle.dhan.fsm(CadenceEvent::FirstFetchDispatched);
                            }
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
                        GateVerdict::RetryAtMs(_) => {
                            metrics::counter!(
                                "tv_cadence_native_retry_total",
                                "lane" => Feed::Dhan.as_str(),
                                "outcome" => "gate_busy_skip"
                            )
                            .increment(1);
                        }
                    }
                }
            }
            // SPEC: force the finalize call with own_path_exhausted: true.
            let CycleState { dhan, .. } = cycle;
            finalize_if_complete(clock.as_ref(), slots, dhan, latch, true, deps.dry_run);
        }
        CycleAction::DhanCutoff => {
            let CycleState { dhan, .. } = cycle;
            // PHASE-B2 (item 2): CADENCE-05 recovery wrap-up reads the
            // PRE-finalize resolved state — the lane's own-path outcome.
            emit_recovery_wrapup(dhan);
            finalize_lane_at_cutoff(clock.as_ref(), slots, dhan, latch, deps.dry_run);
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

/// M11 (audit 2026-07-20, Dim E-2): the runner's OUTER cancel bound sits
/// this far BEYOND the executor's own `deadline_epoch_ms` budget. The
/// executor types its own `Timeout` for the NETWORK phase (its deadline
/// math is unchanged); the outer `tokio::time::timeout` is only the
/// wedge backstop — before this grace it fired at exactly the executor's
/// budget and could cancel the future BETWEEN a succeeded persist/flush
/// and its audit append + fold handoff (persisted rows then mislabeled
/// `Timeout`, forensic row skipped). 1.5s covers the bounded persist
/// tail (ILP flush + audit append via the off-worker flush helpers).
const CADENCE_EXECUTOR_TAIL_GRACE_MS: i64 = 1_500;

/// M9 (audit 2026-07-20, Dim D F1): the count of session-TAIL cycle
/// boundaries that elapsed entirely un-fired when the day loop finds NO
/// further joinable boundary — i.e. the boundaries in
/// `(last_boundary, 15:30:00]` a stall/overrun past session end silently
/// consumed (the in-session `boundary_skipped` arm never sees them). A
/// process that never completed a boundary this session (`None` — e.g. a
/// post-close boot) owns no tail: claiming the whole day would be false.
/// Pure.
#[must_use]
fn unaccounted_session_tail(last_boundary: Option<u32>) -> Option<u32> {
    let lb = last_boundary?;
    let last = super::schedule::CADENCE_LAST_CYCLE_BOUNDARY_SECS_OF_DAY_IST;
    (lb < last).then(|| (last - lb) / 60)
}

/// Bound a chain fetch by the per-request timeout plus the persist-tail
/// grace (Elapsed → `Timeout`; see [`CADENCE_EXECUTOR_TAIL_GRACE_MS`]).
async fn bound_chain_fetch<E: CadenceExecutor>(
    exec: &E,
    req: ChainFetchRequest,
    timeout_ms: i64,
) -> Result<ChainFetchOk, CadenceFetchError> {
    // APPROVED: validated > 0 at boot — the cast is safe.
    #[allow(clippy::cast_sign_loss)]
    let dur = Duration::from_millis(
        timeout_ms
            .max(1)
            .saturating_add(CADENCE_EXECUTOR_TAIL_GRACE_MS) as u64,
    );
    match tokio::time::timeout(dur, exec.fetch_chain(req)).await {
        Ok(r) => r,
        Err(_elapsed) => Err(CadenceFetchError::Timeout),
    }
}

/// Bound a spot fetch by the per-request timeout plus the persist-tail
/// grace (Elapsed → `Timeout`; see [`CADENCE_EXECUTOR_TAIL_GRACE_MS`]).
async fn bound_spot_fetch<E: CadenceExecutor>(
    exec: &E,
    req: SpotFetchRequest,
    timeout_ms: i64,
) -> Result<SpotSnapshot, CadenceFetchError> {
    // APPROVED: validated > 0 at boot — the cast is safe.
    #[allow(clippy::cast_sign_loss)]
    let dur = Duration::from_millis(
        timeout_ms
            .max(1)
            .saturating_add(CADENCE_EXECUTOR_TAIL_GRACE_MS) as u64,
    );
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
fn handle_completion<C, D>(
    clock: &Arc<C>,
    deps: &CadenceRunnerDeps<D>,
    slots: &CycleSlots,
    completion: Completion,
    cycle: &mut CycleState,
    latch: &mut DecisionLatch,
) where
    C: CadenceClock,
    D: CadenceExecutor + 'static,
{
    let cfg = &deps.config;
    let dry_run = deps.dry_run;
    let now_wall = clock.ist_ms_of_day();
    let lane_feed = completion.lane;

    // OUT-OF-BOX GUARD (2026-08-26). Every `Feed` arm below this point is
    // written for a REST cadence lane, and `CycleState` carries a `LaneRun`
    // for Dhan alone -- so a completion naming any other feed has no lane to
    // land in. Until now each of those arms was `unreachable!()`, and under
    // the release profile's `panic = "abort"` that takes the WHOLE trading
    // process down for a case the type system does not forbid:
    // `Completion.lane` is a plain `Feed`, so "structurally unreachable" was
    // caller convention wearing a compiler's clothes. One mis-routed
    // completion -- from a future feed gaining a cadence lane, a test helper,
    // or a refactor that widens `Feed` -- would have aborted the session.
    //
    // Refused here instead: counted, logged with a code, and dropped. The
    // cost of the out-of-box case is now ONE completion rather than the
    // trading day. The arms further down are `return` rather than a
    // best-guess lane, so even with this guard deleted the failure stays a
    // dropped completion instead of a wrong one.
    if !matches!(lane_feed, Feed::Dhan) {
        metrics::counter!(
            "tv_cadence_completion_refused_total",
            "lane" => lane_feed.as_str()
        )
        .increment(1);
        error!(
            code = ErrorCode::Cadence03SchedulerDegraded.code_str(),
            lane = lane_feed.as_str(),
            "cadence completion arrived for a feed that has no cadence lane — dropped"
        );
        return;
    }
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
            // No cadence lane exists for any other feed, and the guard at
            // the top of this function already refused and counted such a
            // completion. This arm is defence in depth: dropping the
            // completion, never guessing the Dhan lane and never aborting
            // the process.
            Feed::Truedata => return,
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
                match result {
                    Ok(ok) => {
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
                        {
                            // Any RateLimited Groww leg (chain OR spot)
                            // marks the fallback-shape cycle dirty
                            // (2026-07-15). Dhan CHAIN 429s deliberately
                            // do NOT feed the SPOT concurrency ladder —
                            // the chain gates are unchanged.
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
                                    true,
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
                            // No cadence lane for this feed; the top-of-function guard already
                            // refused and counted it. Dropped, never a panic.
                            Feed::Truedata => return,
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
                match result {
                    Ok(snap) => {
                        if lane.late_retry_attempts > 0 {
                            // Native spot retry ladder: success after >= 1 native rung.
                            metrics::counter!(
                                "tv_cadence_native_retry_total",
                                "lane" => lane_feed.as_str(),
                                "outcome" => "recovered"
                            )
                            .increment(1);
                        }
                        // WHICH attempt actually delivered (2026-07-31 operator
                        // ask): `burst` = the T+0 volley answered outright;
                        // anything else names the retry rung that won. This is
                        // the ONLY signal that answers "does the broker serve a
                        // just-closed minute at T+0?" — `close_to_data_ms` shows
                        // THAT it got faster, never WHICH attempt got it.
                        metrics::counter!(
                            "tv_cadence_spot_first_success_rung_total",
                            "lane" => lane_feed.as_str(),
                            "rung" => native_rung_label(lane.late_retry_attempts)
                        )
                        .increment(1);
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
                                // No cadence lane for this feed; the top-of-function guard already
                                // refused and counted it. Dropped, never a panic.
                                Feed::Truedata => return,
                            }
                        }
                        // The retry is APPENDED at the next free
                        // rolling-window instant AFTER the last group
                        // anchor (design §1 "spot retries appended",
                        // 2026-07-15 gate change) — an appended retry can
                        // never contend a nominal group's window budget.
                        let mut retry_scheduled = false;
                        if lane_feed == Feed::Dhan {
                            // Native spot retry ladder (item 2 phase B1): an Empty spot
                            // result re-fires at fixed offsets from the minute boundary
                            // instead of advancing the shared retry window.
                            let native_empty =
                                cfg.native_retry_enabled && matches!(err, CadenceFetchError::Empty);
                            // A 429 while the native ladder is active latches the lane and
                            // aborts the remaining rungs this cycle (budget-spent).
                            // CADENCE-01: RateLimited is never `native_empty`, so its one
                            // bounded in-cycle 429 retry keeps the legacy path unchanged.
                            if cfg.native_retry_enabled
                                && lane.late_retry_attempts > 0
                                && matches!(err, CadenceFetchError::RateLimited { .. })
                                && !lane.retry_rate_limited
                            {
                                lane.retry_rate_limited = true;
                                metrics::counter!(
                                    "tv_cadence_native_retry_total",
                                    "lane" => lane_feed.as_str(),
                                    "outcome" => "aborted_429"
                                )
                                .increment(1);
                            }
                            // Clamp to the LAST rung index, derived from the array
                            // itself — never a literal. 2026-07-31: this read
                            // `.min(2)`, the last index of the then-3-rung array;
                            // when the ladder grew to 6 rungs (#1714) that stale
                            // literal pinned every attempt past the 3rd to
                            // CADENCE_NATIVE_RETRY_OFFSETS_MS[2], making the tail
                            // rungs unreachable AND — because that offset is
                            // already in the past by then — collapsing them into
                            // back-to-back immediate re-fires against the broker
                            // (a retry burst that burns the rolling-second budget
                            // and invites the 429s the gates exist to prevent).
                            let rung = (lane.late_retry_attempts as usize)
                                .min(CADENCE_NATIVE_RETRY_OFFSETS_MS.len() - 1);
                            let retry_target = if native_empty {
                                slots
                                    .boundary_ms
                                    .saturating_add(CADENCE_NATIVE_RETRY_OFFSETS_MS[rung])
                                    .max(now_wall)
                            } else {
                                cycle.next_spot_retry_target_ms.max(now_wall)
                            };
                            if !(native_empty && lane.retry_rate_limited)
                                && may_retry_in_cycle(
                                    &err,
                                    // Kill switch OFF => literal `true` (class-blind legacy budget,
                                    // byte-equivalent to the pre-ladder shape). Flag ON => `false`:
                                    // the ladder grants Empty spot legs retry_max.max(3) rungs.
                                    !cfg.native_retry_enabled,
                                    cycle.spot_retries_used[target_idx],
                                    cfg.in_cycle_retry_max,
                                    retry_target,
                                    CADENCE_RETRY_LATENCY_ALLOWANCE_MS,
                                    slots.dhan_cutoff_ms,
                                )
                            {
                                cycle.spot_retries_used[target_idx] += 1;
                                if native_empty {
                                    // Native rung: offset-anchored off the minute boundary -
                                    // the shared retry window cursor is NOT advanced.
                                    lane.late_retry_attempts += 1;
                                } else {
                                    cycle.next_spot_retry_target_ms =
                                        retry_target.saturating_add(CADENCE_SPOT_WINDOW_MS);
                                }
                                retry_scheduled = true;
                                insert_event(
                                    &mut cycle.events,
                                    retry_target,
                                    CycleAction::DhanSpot {
                                        target_idx,
                                        nominal: false,
                                    },
                                );
                            } else if cfg.native_retry_enabled
                                && lane.late_retry_attempts > 0
                                && !matches!(err, CadenceFetchError::RateLimited { .. })
                            {
                                // Budget/cutoff ended the ladder with the leg still failed
                                // after >= 1 native rung. A 429 abort is already counted as
                                // aborted_429 - never double-counted here.
                                metrics::counter!(
                                    "tv_cadence_native_retry_total",
                                    "lane" => lane_feed.as_str(),
                                    "outcome" => "exhausted"
                                )
                                .increment(1);
                            }
                        }
                        // L3 (2026-07-15): the DEFERRED per-leg fallback
                        // for an in-flight-skipped SPOT leg — see the
                        // chain arm above.
                        let terminal = match lane_feed {
                            Feed::Dhan => !retry_scheduled,
                            // No cadence lane for this feed; the top-of-function guard already
                            // refused and counted it. Dropped, never a panic.
                            Feed::Truedata => return,
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
    let CycleState { dhan, .. } = cycle;
    finalize_if_complete(clock.as_ref(), slots, dhan, latch, dhan_exhausted, dry_run);
}

/// Is the lane's OWN fetch path exhausted for this cycle? TRUE when the
/// lane has no in-flight fetch AND no remaining scheduled own event
/// (primaries, retries — cutoffs are not own
/// work). Only then may the fallback rungs run before the cutoff.
fn lane_own_path_exhausted(feed: Feed, cycle: &CycleState) -> bool {
    let lane = match feed {
        Feed::Dhan => &cycle.dhan,
        // No cadence lane exists for this feed, so it has no outstanding
        // work of its own -- which is precisely what "exhausted" means
        // here. `true` is therefore the correct answer and not a fallback:
        // it lets the shared fallback rungs proceed rather than waiting
        // forever on a lane that will never report.
        Feed::Truedata => return true,
    };
    if lane.inflight > 0 {
        return false;
    }
    !cycle.events.iter().any(|(_, action)| match feed {
        Feed::Dhan => matches!(
            action,
            CycleAction::DhanChain { .. } | CycleAction::DhanSpot { .. }
        ),
        // Same reasoning: no lane means no events of its own to find.
        Feed::Truedata => false,
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
    latch: &mut DecisionLatch,
    own_path_exhausted: bool,
    dry_run: bool,
) {
    if !lane.enabled || lane.resolved || lane.state != CadenceState::Fetching {
        return;
    }
    let now_wall = clock.ist_ms_of_day();
    let cutoff = slots.dhan_cutoff_ms;
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
        // Rung 2 (cross-source fill from a second lane) was REMOVED with the
        // Groww lane 2026-08-21 — a single-lane cadence has no donor.
        // Rung 3: the lane.s own chain-embedded spot.
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
    // The ADVISORY cross-broker coherence band was REMOVED with the Groww
    // lane 2026-08-21 — it compared two lanes; one lane has nothing to
    // compare against.
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
        // ADVISORY coherence band (H3/H2-partial, audit 2026-07-20): the
        // chain's embedded underlying spot vs the lane's resolved spot
        // cell. Chain bodies carry no vendor timestamp or instrument
        // echo, so a >0.5% disagreement is the honest proxy for a
        // vendor-stale chain body or a wrong-instrument response. Flag +
        // counter only — never decision-blocking, never arming.
        if let (Some(embedded_paise), Some(cell)) = (
            lane.asm
                .chain(*u)
                .and_then(|c| c.embedded_spot)
                .and_then(tickvault_common::moneyness::price_to_paise_guarded),
            lane.asm.spot(*u),
        ) && spots_diverge_paise(embedded_paise, cell.spot_paise)
        {
            // Coalesced ADVISORY emission (2026-07-20, adversarial review):
            // decoupled from CADENCE-01 — one plain info! per lane per
            // cycle (first offender named), NO ErrorCode, counter kept.
            if !lane.flags.chain_spot_divergence {
                info!(
                    kind = "chain_spot_divergence",
                    lane = feed.as_str(),
                    underlying = u.as_str(),
                    embedded_spot_paise = embedded_paise,
                    cell_spot_paise = cell.spot_paise,
                    delta_paise = (embedded_paise - cell.spot_paise).abs(),
                    "cadence advisory: chain-embedded spot diverged from the \
                     resolved spot cell beyond the 0.5% band (info-only, \
                     coalesced — not a CADENCE-01 stage)"
                );
            }
            lane.flags.chain_spot_divergence = true;
            metrics::counter!(
                "tv_cadence_chain_spot_divergence_total",
                "lane" => feed.as_str(),
                "underlying" => u.as_str()
            )
            .increment(1);
        }
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
        // SEAM(#1688): resolution provenance recorded exactly once per site.
        lane.resolution = Some(resolution_token(lane.late_retry_attempts));
        lane.resolved = true;
        debug_assert!(lane.resolution.is_some(), "resolution set before resolved");
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
    // SEAM(#1688): resolution provenance recorded exactly once per site.
    lane.resolution = Some(resolution_token(lane.late_retry_attempts));
    lane.resolved = true;
    debug_assert!(lane.resolution.is_some(), "resolution set before resolved");
}

/// Label for WHICH attempt first delivered a spot — the `rung` dimension of
/// `tv_cadence_spot_first_success_rung_total` (2026-07-31 operator ask:
/// "which attempt won?").
///
/// `attempts` is the lane's native-retry count AT THE MOMENT OF SUCCESS, so 0
/// means the T+0 burst itself answered — the single most important reading,
/// because whether a broker serves a just-closed minute at T+0 has never been
/// observed (zero empty responses on record as of 2026-07-31).
///
/// Labels are `&'static str` and derived by INDEX, so a future edit to
/// [`CADENCE_NATIVE_RETRY_OFFSETS_MS`] can never silently mislabel a rung —
/// an index past the table degrades to `beyond_table` rather than pretending
/// to name an offset. O(1), zero-allocation.
fn native_rung_label(attempts: u32) -> &'static str {
    // Kept in lockstep with CADENCE_NATIVE_RETRY_OFFSETS_MS by
    // `test_native_rung_label_covers_every_rung_in_the_offsets_table`.
    const LABELS: [&str; 6] = [
        "retry_1_5ms",
        "retry_2_300ms",
        "retry_3_1000ms",
        "retry_4_2000ms",
        "retry_5_3000ms",
        "retry_6_3800ms",
    ];
    if attempts == 0 {
        return "burst";
    }
    LABELS
        .get((attempts as usize) - 1)
        .copied()
        .unwrap_or("beyond_table")
}

/// Resolution provenance vocabulary — LOCKED: "native_late_retry" |
/// "native_first_try".
///
/// The third token, "cross_fill", was retired with the second broker on
/// 2026-08-21: it meant "this lane's minute was filled from the other
/// broker's same-cycle data", which one lane cannot do.
fn resolution_token(late_retry_attempts: u32) -> &'static str {
    if late_retry_attempts > 0 {
        "native_late_retry"
    } else {
        "native_first_try"
    }
}

/// Cutoff handling: one final finalize attempt, else HONEST-SKIP with
/// the precise reason — never a late decision (design §5).
fn finalize_lane_at_cutoff<C: CadenceClock>(
    clock: &C,
    slots: &CycleSlots,
    lane: &mut LaneRun,
    latch: &mut DecisionLatch,
    dry_run: bool,
) {
    if !lane.enabled || lane.resolved {
        return;
    }
    // Last chance: the cross-fill window is valid up to AND INCLUDING
    // the cutoff instant (a cutoff event popping even 1ms late finds the
    // finalize guard refusing — fail-closed, the skip below owns it).
    finalize_if_complete(clock, slots, lane, latch, true, dry_run);
    if lane.resolved {
        return;
    }
    let now_wall = clock.ist_ms_of_day();
    // Literally nothing usable on EITHER side ⇒ both_sources_dead; a
    // partially-assembled lane at its cutoff ⇒ cutoff.
    let lane_empty = ChainUnderlying::ALL
        .iter()
        .all(|u| lane.asm.chain(*u).is_none() && lane.asm.spot(*u).is_none());
    let reason = if lane_empty {
        SkipReason::BothSourcesDead
    } else {
        SkipReason::Cutoff
    };
    if !latch.try_latch(lane.asm.feed, lane.asm.cycle_minute_ist) {
        // SEAM(#1688): resolution provenance recorded exactly once per site.
        lane.resolution = Some(resolution_token(lane.late_retry_attempts));
        lane.resolved = true;
        debug_assert!(lane.resolution.is_some(), "resolution set before resolved");
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
    // SEAM(#1688): resolution provenance recorded exactly once per site.
    lane.resolution = Some(resolution_token(lane.late_retry_attempts));
    lane.resolved = true;
    debug_assert!(lane.resolution.is_some(), "resolution set before resolved");
}

/// PHASE-B2 (item 2): CADENCE-05 recovery wrap-up — LOG-SINK-ONLY, once per
/// lane per minute (each cutoff action fires exactly once per cycle). Called
/// at cycle teardown (the cutoff handlers) BEFORE `finalize_lane_at_cutoff`,
/// so the guard reads the lane's own-path outcome: the native retry ladder
/// ran (`late_retry_attempts > 0`) and the lane still enters cutoff
/// unresolved — recovery degraded to the cross-fill/cutoff decision floor.
/// No Telegram, no NotificationEvent, no alarm wiring.
fn emit_recovery_wrapup(lane: &LaneRun) {
    if !lane.enabled || lane.resolved || lane.late_retry_attempts == 0 {
        return;
    }
    let stage = if lane.retry_rate_limited {
        "retry_rate_limited"
    } else {
        "retry_still_empty"
    };
    error!(
        code = ErrorCode::Cadence05RecoveryDegraded.code_str(),
        stage,
        lane = lane.asm.feed.as_str(),
        cycle_minute_ist = lane.asm.cycle_minute_ist,
        late_retry_attempts = lane.late_retry_attempts,
        "CADENCE-05: native retries exhausted without recovery — cutoff/cross-fill is the floor"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// M9 (audit 2026-07-20): the session-tail accounting owns exactly
    /// the boundaries in `(last_boundary, 15:30:00]` — and refuses to
    /// claim a day it never served.
    #[test]
    fn unaccounted_session_tail_cases() {
        let last = super::super::schedule::CADENCE_LAST_CYCLE_BOUNDARY_SECS_OF_DAY_IST;
        // A post-close boot that never completed a boundary owns no tail.
        assert_eq!(unaccounted_session_tail(None), None);
        // A healthy day (final boundary completed) has no tail.
        assert_eq!(unaccounted_session_tail(Some(last)), None);
        // Stalled after 15:28 → the 15:29 and 15:30 boundaries dropped.
        assert_eq!(unaccounted_session_tail(Some(last - 120)), Some(2));
        // Stalled right after the penultimate boundary → exactly one.
        assert_eq!(unaccounted_session_tail(Some(last - 60)), Some(1));
    }

    /// M11 (audit 2026-07-20): the outer cancel bound is STRICTLY beyond
    /// the executor's own budget (additive grace, saturating), so it can
    /// no longer sever a completed persist from its audit row.
    #[test]
    fn executor_tail_grace_is_bounded_and_additive() {
        assert_eq!(CADENCE_EXECUTOR_TAIL_GRACE_MS, 1_500);
        let timeout_ms: i64 = 5_000;
        let outer = timeout_ms
            .max(1)
            .saturating_add(CADENCE_EXECUTOR_TAIL_GRACE_MS);
        assert!(outer > timeout_ms);
        assert_eq!(outer, 6_500);
        // Saturating: a pathological i64::MAX budget never overflows.
        assert_eq!(
            i64::MAX.saturating_add(CADENCE_EXECUTOR_TAIL_GRACE_MS),
            i64::MAX
        );
    }

    /// 2026-07-20 (adversarial review): the ADVISORY divergence flag is
    /// DECOUPLED from CADENCE-01 — alone it never reads as a degrade
    /// (`any()` false, `stages()` empty), so a routine 0.5% divergence can
    /// never false-fire the High degrade line. (The second, cross-BROKER
    /// divergence flag retired with the second broker on 2026-08-21.)
    #[test]
    fn divergence_flags_alone_never_degrade() {
        let flags = DegradeFlags {
            chain_spot_divergence: true,
            ..DegradeFlags::default()
        };
        assert!(!flags.any());
        assert!(flags.stages().is_empty());
        // And a clean cycle stays clean.
        assert!(!DegradeFlags::default().any());
        assert!(DegradeFlags::default().stages().is_empty());
    }

    /// 2026-07-20 (adversarial review): a REAL degrade flag still arms
    /// CADENCE-01, and the advisory divergence flags never leak into the
    /// coalesced stage string beside it.
    #[test]
    fn real_degrade_excludes_divergence_stages() {
        let flags = DegradeFlags {
            fetch_failed: true,
            chain_spot_divergence: true,
            ..DegradeFlags::default()
        };
        assert!(flags.any());
        let s = flags.stages();
        assert_eq!(s, "fetch_failed");
        assert!(!s.contains("chain_spot_divergence"));
    }

    // ⚠ ORPHANED DOC — 2026-08-11. The block below documents a PHASE-B2
    // (item 2) kill-switch-OFF byte-equivalence pin: with
    // `native_retry_enabled = false` the runner passes `leg_is_chain = true`
    // for spots, so every class keeps the legacy class-blind budget and the
    // legacy target expression survives in the source; Malformed is the ONE
    // spec-sanctioned exception on BOTH arms (never retried, budget 0).
    //
    // ITS FUNCTION BODY DOES NOT EXIST. The doc and a bare `#[test]` sat here
    // with no `fn` after them, so the attribute bound to the NEXT test
    // instead — which rustc reported only as a `duplicated attribute`
    // warning inside a test module, where `cargo clippy --workspace` (no
    // `--tests`) never looks. The described pin has therefore been absent for
    // however long, while its documentation read as though it were enforced.
    //
    // Converted to a plain comment rather than deleted: deleting it would
    // erase the evidence that a regression pin was lost, which is the only
    // reason anyone would know to restore it. Reconstructing the body is a
    // cadence-lane task with its own scope — flagged, deliberately not
    // guessed at here, because a re-written pin that asserts something
    // subtly different from the original is worse than a missing one.
    /// 2026-07-31 REGRESSION PIN. The rung index was clamped with a literal
    /// `.min(2)` — the last index of the then-3-rung table. When #1714 grew
    /// the ladder to 6 rungs that literal shipped unchanged, so every attempt
    /// past the 3rd resolved to `CADENCE_NATIVE_RETRY_OFFSETS_MS[2]`: the
    /// 2000/3000/3800 rungs became UNREACHABLE, and because index 2's offset
    /// is already in the past by then, `.max(now_wall)` collapsed those three
    /// attempts into back-to-back immediate re-fires — a retry burst against
    /// the broker that burns the rolling-second budget and invites the very
    /// 429s the gates exist to prevent.
    ///
    /// The clamp MUST derive from the table's own length so growing the
    /// ladder can never again strand its tail.
    #[test]
    fn test_native_rung_clamp_tracks_the_offsets_table_length() {
        let src = include_str!("runner.rs");
        assert!(
            src.contains("min(CADENCE_NATIVE_RETRY_OFFSETS_MS.len() - 1)"),
            "the native rung clamp must derive from the offsets table length"
        );
        // Every rung index must be reachable, and each must map to a DISTINCT
        // offset — the property the literal clamp silently destroyed.
        let last = CADENCE_NATIVE_RETRY_OFFSETS_MS.len() - 1;
        let reached: Vec<i64> = (0..=last)
            .map(|attempts| CADENCE_NATIVE_RETRY_OFFSETS_MS[attempts.min(last)])
            .collect();
        assert_eq!(
            reached,
            CADENCE_NATIVE_RETRY_OFFSETS_MS.to_vec(),
            "every rung must be reachable — a stranded tail means silent immediate re-fires"
        );
    }

    /// The `rung` label vocabulary must cover EVERY entry of the offsets
    /// table (plus the T+0 burst), so a grown ladder can never emit an
    /// unnamed rung into the metric.
    #[test]
    fn test_native_rung_label_covers_every_rung_in_the_offsets_table() {
        assert_eq!(native_rung_label(0), "burst");
        for attempts in 1..=CADENCE_NATIVE_RETRY_OFFSETS_MS.len() {
            let label = native_rung_label(u32::try_from(attempts).expect("rung count fits u32"));
            assert_ne!(
                label, "beyond_table",
                "rung {attempts} has no label — the label table drifted from \
                 CADENCE_NATIVE_RETRY_OFFSETS_MS"
            );
            assert!(
                label.starts_with("retry_"),
                "unexpected label {label} for rung {attempts}"
            );
        }
        // One past the table degrades honestly instead of mislabelling.
        let past = u32::try_from(CADENCE_NATIVE_RETRY_OFFSETS_MS.len() + 1).expect("fits u32");
        assert_eq!(native_rung_label(past), "beyond_table");
    }

    #[test]
    fn test_native_retry_kill_switch_off_is_legacy_class_blind() {
        use crate::cadence::ladder::late_retry_budget;

        // OFF (leg_is_chain=true for spots): legacy class-blind budget.
        assert_eq!(late_retry_budget(&CadenceFetchError::Empty, true, 1), 1);
        assert_eq!(late_retry_budget(&CadenceFetchError::Timeout, true, 1), 1);
        assert_eq!(late_retry_budget(&CadenceFetchError::Transport, true, 1), 1);
        assert_eq!(
            late_retry_budget(
                &CadenceFetchError::RateLimited {
                    retry_after_ms: None
                },
                true,
                1
            ),
            1
        );
        assert_eq!(
            late_retry_budget(&CadenceFetchError::QueueDelay, true, 1),
            1
        );
        // ON (spot leg): Empty gets the FULL native ladder. Bound to the
        // constant, not a literal — 2026-07-31 grew the ladder 3 -> 5
        // rungs (two EARLY rungs prepended for the T+5 burst move) and a
        // literal here would have silently disagreed with the array that
        // `constants.rs` already pins verbatim.
        assert_eq!(
            late_retry_budget(&CadenceFetchError::Empty, false, 1),
            tickvault_common::constants::CADENCE_NATIVE_RETRY_MAX_ATTEMPTS as u32
        );
        // Malformed is NEVER retried — kill switch ON or OFF.
        assert_eq!(late_retry_budget(&CadenceFetchError::Malformed, true, 1), 0);
        assert_eq!(
            late_retry_budget(&CadenceFetchError::Malformed, false, 1),
            0
        );
        // Source pins: the legacy target expression + the cfg gate + the ONE
        // combined L3 malformed filter.
        let src = include_str!("runner.rs");
        assert!(src.contains("cycle.next_spot_retry_target_ms.max(now_wall)"));
        assert!(src.contains("!cfg.native_retry_enabled"));
    }

    /// ITEM 3 ratchet: LaneRun's ctor initializes the resolution token to None.
    #[test]
    fn ratchet_lane_run_ctor_initializes_resolution_none() {
        let src = include_str!("runner.rs");
        let needle = ["resolution", ": None,"].concat();
        assert!(
            src.contains(needle.as_str()),
            "LaneRun ctor must initialize resolution: None"
        );
    }

    /// ITEM 3 ratchet: exactly one resolution assignment IMMEDIATELY before
    /// each resolved-site (exactly-one-resolution per slot per minute).
    #[test]
    fn ratchet_resolution_set_exactly_before_each_resolved_site() {
        let src = include_str!("runner.rs");
        let resolved = ["lane.resolved", " = true;"].concat();
        let seam = ["// SEAM(", "#1688):"].concat();
        let set = ["lane.resolution", ".is_some()"].concat();
        let n = src.matches(resolved.as_str()).count();
        assert_eq!(n, 6, "six genuine lane-resolution sites");
        assert_eq!(
            src.matches(seam.as_str()).count(),
            n,
            "one SEAM comment per site"
        );
        assert_eq!(
            src.matches(set.as_str()).count(),
            n,
            "one debug_assert per site"
        );
    }
}
