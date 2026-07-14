//! Cadence runner end-to-end under PAUSED tokio time (design §11
//! Integration row): the real `run_cadence_loop` + a scripted recording
//! executor drive full cycles — dry-run Empty semantics, the Groww
//! burst→verdict→fallback path, and the pure instant-decision /
//! honest-skip contracts.
//!
//! No metrics assertions: adding a metrics debugging recorder would need
//! a NEW dev-dependency (operator approval required), so the observable
//! surface here is the recording executor's request log + the loop's
//! typed exit — the decision/skip emissions themselves are pinned at the
//! pure layer (`DecisionLatch` + `emit_decision`) below.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;

use chrono::NaiveDate;
use tickvault_common::config::{CadenceConfig, TradingConfig};
use tickvault_common::feed::Feed;
use tickvault_common::trading_calendar::TradingCalendar;
use tickvault_core::cadence::assembly::MoneynessFold;
use tickvault_core::cadence::assembly::{ChainCell, ChainProvenance, LaneAssembly, SpotProvenance};
use tickvault_core::cadence::decision::{
    DecisionLatch, DecisionOutcome, DecisionSnapshot, SkipReason, emit_decision,
};
use tickvault_core::cadence::executor::{
    CadenceExecutor, CadenceFetchError, ChainFetchOk, ChainFetchRequest, SpotFetchRequest,
    SpotSnapshot, SpotTarget,
};
use tickvault_core::cadence::runner::{
    CadenceClock, CadenceRunnerDeps, LoopExit, run_cadence_loop, spawn_supervised_cadence_runner,
};
use tickvault_core::pipeline::chain_snapshot::ChainUnderlying;
use tokio::sync::Notify;

// ---------------------------------------------------------------------------
// Test clock + fixtures
// ---------------------------------------------------------------------------

/// Paused-tokio-time clock: wall/epoch = base + tokio elapsed, monotonic =
/// tokio elapsed. Under `start_paused = true` every runner sleep
/// auto-advances, so full 15s cycles run in real milliseconds.
struct TestClock {
    anchor: tokio::time::Instant,
    base_wall_ms: i64,
    date: NaiveDate,
}

impl CadenceClock for TestClock {
    fn ist_ms_of_day(&self) -> i64 {
        // APPROVED: paused-test elapsed fits i64 comfortably.
        #[allow(clippy::cast_possible_truncation)]
        {
            self.base_wall_ms + self.anchor.elapsed().as_millis() as i64
        }
    }

    fn ist_date(&self) -> NaiveDate {
        self.date
    }

    fn monotonic_ms(&self) -> i64 {
        // APPROVED: paused-test elapsed fits i64 comfortably.
        #[allow(clippy::cast_possible_truncation)]
        {
            self.anchor.elapsed().as_millis() as i64
        }
    }

    fn epoch_ms(&self) -> i64 {
        self.ist_ms_of_day()
    }
}

fn test_calendar() -> Arc<TradingCalendar> {
    let cfg = TradingConfig {
        market_open_time: "09:00:00".to_string(),
        market_close_time: "15:30:00".to_string(),
        order_cutoff_time: "15:29:00".to_string(),
        data_collection_start: "09:00:00".to_string(),
        data_collection_end: "15:30:00".to_string(),
        timezone: "Asia/Kolkata".to_string(),
        max_orders_per_second: 10,
        nse_holidays: vec![],
        muhurat_trading_dates: vec![],
        nse_mock_trading_dates: vec![],
    };
    Arc::new(TradingCalendar::from_config(&cfg).expect("calendar must build"))
}

/// One recorded executor call.
#[derive(Clone, Debug, PartialEq, Eq)]
enum RecordedCall {
    Chain {
        feed: Feed,
        underlying: ChainUnderlying,
        cycle_minute_ist: u32,
    },
    Spot {
        feed: Feed,
        target: SpotTarget,
        cycle_minute_ist: u32,
    },
}

/// Scripted recording executor: logs every request; outcome decided by
/// the injected chain/spot verdict fns (call-count-aware for retry
/// scripting).
struct RecordingExecutor {
    log: Arc<Mutex<Vec<RecordedCall>>>,
    chain_verdict: fn(&ChainFetchRequest, usize) -> Result<ChainFetchOk, CadenceFetchError>,
    spot_verdict: fn(&SpotFetchRequest, usize) -> Result<SpotSnapshot, CadenceFetchError>,
}

impl RecordingExecutor {
    fn count_chain(&self, feed: Feed, underlying: ChainUnderlying) -> usize {
        // APPROVED (test-only): a poisoned mutex here means a sibling
        // assertion already failed — propagate the panic.
        #[allow(clippy::unwrap_used)]
        self.log
            .lock()
            .unwrap()
            .iter()
            .filter(|c| {
                matches!(c, RecordedCall::Chain { feed: f, underlying: u, .. }
                    if *f == feed && *u == underlying)
            })
            .count()
    }
}

impl CadenceExecutor for RecordingExecutor {
    fn fetch_chain(
        &self,
        req: ChainFetchRequest,
    ) -> impl std::future::Future<Output = Result<ChainFetchOk, CadenceFetchError>> + Send {
        let prior;
        {
            // APPROVED (test-only): poisoned mutex propagates the panic.
            #[allow(clippy::unwrap_used)]
            let mut log = self.log.lock().unwrap();
            prior = log
                .iter()
                .filter(|c| {
                    matches!(c, RecordedCall::Chain { feed, underlying, .. }
                        if *feed == req.feed && *underlying == req.underlying)
                })
                .count();
            log.push(RecordedCall::Chain {
                feed: req.feed,
                underlying: req.underlying,
                cycle_minute_ist: req.cycle_minute_ist,
            });
        }
        let verdict = (self.chain_verdict)(&req, prior);
        async move { verdict }
    }

    fn fetch_spot(
        &self,
        req: SpotFetchRequest,
    ) -> impl std::future::Future<Output = Result<SpotSnapshot, CadenceFetchError>> + Send {
        let prior;
        {
            // APPROVED (test-only): poisoned mutex propagates the panic.
            #[allow(clippy::unwrap_used)]
            let mut log = self.log.lock().unwrap();
            prior = log
                .iter()
                .filter(|c| {
                    matches!(c, RecordedCall::Spot { feed, target, .. }
                        if *feed == req.feed && *target == req.target)
                })
                .count();
            log.push(RecordedCall::Spot {
                feed: req.feed,
                target: req.target,
                cycle_minute_ist: req.cycle_minute_ist,
            });
        }
        let verdict = (self.spot_verdict)(&req, prior);
        async move { verdict }
    }
}

/// 09:15:50 IST — 10s before the first joinable boundary (09:16:00).
const BASE_WALL_MS: i64 = (9 * 3600 + 15 * 60 + 50) * 1_000;
/// The first cycle's decided minute (09:15:00, seconds-of-day).
const FIRST_CYCLE_MINUTE: u32 = 9 * 3600 + 15 * 60;

fn deps_with(
    exec: Arc<RecordingExecutor>,
    dhan_on: bool,
    groww_on: bool,
) -> (
    CadenceRunnerDeps<RecordingExecutor, RecordingExecutor>,
    Arc<Notify>,
) {
    let shutdown = Arc::new(Notify::new());
    let deps = CadenceRunnerDeps {
        config: CadenceConfig::default(),
        calendar: test_calendar(),
        dhan_executor: Arc::clone(&exec),
        groww_executor: exec,
        dhan_enabled: Arc::new(AtomicBool::new(dhan_on)),
        groww_enabled: Arc::new(AtomicBool::new(groww_on)),
        shutdown: Arc::clone(&shutdown),
    };
    (deps, shutdown)
}

fn empty_chain(_req: &ChainFetchRequest, _prior: usize) -> Result<ChainFetchOk, CadenceFetchError> {
    Err(CadenceFetchError::Empty)
}

fn empty_spot(_req: &SpotFetchRequest, _prior: usize) -> Result<SpotSnapshot, CadenceFetchError> {
    Err(CadenceFetchError::Empty)
}

// ---------------------------------------------------------------------------
// Runner end-to-end (paused time)
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn test_cadence_runner_dry_run_full_cycle_emits_decisions_or_skips() {
    // DRY-RUN semantics (the DryRunLoggingExecutor contract — every fire
    // logged, Empty returned, prices NEVER synthesized) via the recording
    // twin so the fire pattern is assertable. The cycle must complete
    // end-to-end: every primary slot fired, honest-skips latched at the
    // cutoffs (pure-layer pinned below), the loop alive for the NEXT
    // cycle, and shutdown exits typed.
    let log = Arc::new(Mutex::new(Vec::new()));
    let exec = Arc::new(RecordingExecutor {
        log: Arc::clone(&log),
        chain_verdict: empty_chain,
        spot_verdict: empty_spot,
    });
    let (deps, shutdown) = deps_with(Arc::clone(&exec), true, true);
    let clock = Arc::new(TestClock {
        anchor: tokio::time::Instant::now(),
        base_wall_ms: BASE_WALL_MS,
        date: NaiveDate::from_ymd_opt(2026, 7, 14).expect("valid date"), // Tuesday
    });
    let task = tokio::spawn(run_cadence_loop(clock, deps));

    // Let the first cycle fully elapse (cutoff T+15s ⇒ 25s from base) —
    // paused time auto-advances through every runner sleep.
    // Two full cycles: cycle 1 closes 09:16:00 (cutoff +15s), cycle 2
    // closes 09:17:00 — 100s of paused time covers both with margin.
    for _ in 0..100 {
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
    shutdown.notify_waiters();
    let exit = tokio::time::timeout(std::time::Duration::from_secs(120), task)
        .await
        .expect("runner must exit after shutdown")
        .expect("runner task must not panic");
    assert_eq!(exit, LoopExit::Shutdown);

    // APPROVED (test-only): poisoned mutex propagates the panic.
    #[allow(clippy::unwrap_used)]
    let calls = log.lock().unwrap().clone();
    let first_cycle: Vec<_> = calls
        .iter()
        .filter(|c| match c {
            RecordedCall::Chain {
                cycle_minute_ist, ..
            }
            | RecordedCall::Spot {
                cycle_minute_ist, ..
            } => *cycle_minute_ist == FIRST_CYCLE_MINUTE,
        })
        .collect();
    // Dhan lane: 3 chain primaries + up to 3 grid retries; 4 spot
    // singles + up to 4 appended retries (Empty is retryable).
    // Groww lane: the 7-parallel burst + the all-failed fallback (7).
    let dhan_chains = first_cycle
        .iter()
        .filter(|c| {
            matches!(
                c,
                RecordedCall::Chain {
                    feed: Feed::Dhan,
                    ..
                }
            )
        })
        .count();
    let dhan_spots = first_cycle
        .iter()
        .filter(|c| {
            matches!(
                c,
                RecordedCall::Spot {
                    feed: Feed::Dhan,
                    ..
                }
            )
        })
        .count();
    let groww_chains = first_cycle
        .iter()
        .filter(|c| {
            matches!(
                c,
                RecordedCall::Chain {
                    feed: Feed::Groww,
                    ..
                }
            )
        })
        .count();
    let groww_spots = first_cycle
        .iter()
        .filter(|c| {
            matches!(
                c,
                RecordedCall::Spot {
                    feed: Feed::Groww,
                    ..
                }
            )
        })
        .count();
    // EXACT counts (deterministic all-Empty script): Empty IS retryable
    // in-cycle (may_retry_in_cycle) and every retry slot lands inside the
    // :15 cutoff — 3 chain primaries + 3 grid retries, 4 spot singles +
    // 4 appended retries. Exact equality pins that the RUNNER's retry
    // insertion (handle_completion → insert_event) actually fires:
    // deleting it would read 3/4 here.
    assert_eq!(
        dhan_chains, 6,
        "Dhan chains: 3 primaries + 3 grid retries (all Empty)"
    );
    assert_eq!(
        dhan_spots, 8,
        "Dhan spots: 4 singles + 4 appended retries (all Empty)"
    );
    assert_eq!(
        groww_chains, 6,
        "Groww chains: 3 burst + 3 fallback (all failed)"
    );
    assert_eq!(
        groww_spots, 8,
        "Groww spots: 4 burst + 4 fallback (all failed)"
    );
    // The loop stayed alive past cycle 1 (dry-run skips are never a
    // wedge): the SECOND cycle's fires exist too.
    assert!(
        calls.iter().any(|c| match c {
            RecordedCall::Chain {
                cycle_minute_ist, ..
            }
            | RecordedCall::Spot {
                cycle_minute_ist, ..
            } => *cycle_minute_ist == FIRST_CYCLE_MINUTE + 60,
        }),
        "the runner must roll into the next cycle after a skipped one"
    );
}

#[tokio::test(start_paused = true)]
async fn test_groww_burst_fallback_refetches_only_failures() {
    // Script: Groww BANKNIFTY chain fails its burst leg (Transport);
    // every other leg is Ok. The T+800 verdict must refetch ONLY the
    // failed leg — successes are never re-fetched (design §1).
    fn chain_verdict(
        req: &ChainFetchRequest,
        prior: usize,
    ) -> Result<ChainFetchOk, CadenceFetchError> {
        if req.underlying == ChainUnderlying::Banknifty && prior == 0 {
            return Err(CadenceFetchError::Transport);
        }
        Ok(ChainFetchOk {
            underlying_spot: Some(24_500.0),
            published_to_registry: false,
        })
    }
    fn spot_verdict(
        req: &SpotFetchRequest,
        _prior: usize,
    ) -> Result<SpotSnapshot, CadenceFetchError> {
        Ok(SpotSnapshot {
            price: 24_500.0,
            source_minute_ist: req.cycle_minute_ist,
            received_at_epoch_ms: 0,
        })
    }
    let log = Arc::new(Mutex::new(Vec::new()));
    let exec = Arc::new(RecordingExecutor {
        log: Arc::clone(&log),
        chain_verdict,
        spot_verdict,
    });
    // Groww-only lane (Dhan disabled — isolates the burst semantics).
    let (deps, shutdown) = deps_with(Arc::clone(&exec), false, true);
    let clock = Arc::new(TestClock {
        anchor: tokio::time::Instant::now(),
        base_wall_ms: BASE_WALL_MS,
        date: NaiveDate::from_ymd_opt(2026, 7, 14).expect("valid date"),
    });
    let task = tokio::spawn(run_cadence_loop(clock, deps));
    for _ in 0..30 {
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
    shutdown.notify_waiters();
    let exit = tokio::time::timeout(std::time::Duration::from_secs(120), task)
        .await
        .expect("runner must exit after shutdown")
        .expect("runner task must not panic");
    assert_eq!(exit, LoopExit::Shutdown);

    assert_eq!(
        exec.count_chain(Feed::Groww, ChainUnderlying::Banknifty),
        2,
        "the failed BANKNIFTY leg is refetched exactly once by the verdict"
    );
    assert_eq!(
        exec.count_chain(Feed::Groww, ChainUnderlying::Nifty),
        1,
        "a successful burst leg is never refetched"
    );
    assert_eq!(
        exec.count_chain(Feed::Groww, ChainUnderlying::Sensex),
        1,
        "a successful burst leg is never refetched"
    );
}

// ---------------------------------------------------------------------------
// Pure-layer decision contracts (design-named)
// ---------------------------------------------------------------------------

#[test]
fn test_decision_fires_instant_predicate_completes() {
    // The predicate (3 chains + 3 spots, VIX advisory) flips TRUE on the
    // LAST required cell — and the latch admits the decision at that
    // exact event, not at any timer.
    let mut a = LaneAssembly::new(Feed::Groww, FIRST_CYCLE_MINUTE, 33_360_000);
    let mut latch = DecisionLatch::new();
    for u in ChainUnderlying::ALL {
        assert!(!a.is_data_complete(), "incomplete before every chain");
        a.record_chain(
            *u,
            ChainCell {
                provenance: ChainProvenance::OwnFetch,
                source_feed: Feed::Groww,
                published_to_registry: true,
                fetched_at_ms: 33_360_100,
                minute_ist: FIRST_CYCLE_MINUTE,
                embedded_spot: None,
            },
        );
    }
    for (i, target) in [SpotTarget::Nifty, SpotTarget::BankNifty, SpotTarget::Sensex]
        .iter()
        .enumerate()
    {
        assert!(
            !a.is_data_complete(),
            "incomplete before spot #{i} — the decision must not fire early"
        );
        a.record_spot(
            *target,
            24_500.0,
            SpotProvenance::OwnFetch,
            33_360_200,
            FIRST_CYCLE_MINUTE,
        );
    }
    // The INSTANT the last spot landed the predicate is true (VIX still
    // absent — advisory only) and the latch emits exactly once.
    assert!(a.is_data_complete());
    assert!(a.vix_missing());
    assert!(latch.try_latch(Feed::Groww, FIRST_CYCLE_MINUTE));
    assert!(!latch.try_latch(Feed::Groww, FIRST_CYCLE_MINUTE));
}

#[test]
fn test_honest_skip_at_cutoff_emits_alert_once() {
    // A lane that reaches its cutoff incomplete honest-skips: the latch
    // admits exactly ONE Skipped emission per (lane, cycle) — re-entry
    // (a late completion racing the cutoff) is refused, and the skip
    // snapshot always carries a typed reason (Rule 11 — never rendered
    // OK; the emit path itself must not panic).
    let mut latch = DecisionLatch::new();
    let snap = DecisionSnapshot {
        lane: Feed::Dhan,
        cycle_minute_ist: FIRST_CYCLE_MINUTE,
        outcome: DecisionOutcome::Skipped(SkipReason::Cutoff),
        vix_missing: true,
        post_close: false,
        latency_ms: 15_000,
        moneyness: [MoneynessFold::default(); ChainUnderlying::COUNT],
        spot_provenance: [None; ChainUnderlying::COUNT],
    };
    assert!(latch.try_latch(snap.lane, snap.cycle_minute_ist));
    emit_decision(&snap); // one CADENCE-02-coded emission
    assert!(
        !latch.try_latch(snap.lane, snap.cycle_minute_ist),
        "a second cutoff/late-completion race can never re-emit"
    );
    if let DecisionOutcome::Skipped(reason) = snap.outcome {
        assert_eq!(reason.as_str(), "cutoff");
    }
}

// ---------------------------------------------------------------------------
// Both-lanes-disabled park + supervisor shutdown
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn test_cadence_runner_reenable_after_both_disabled_park_still_cycles() {
    // A transient dual-disable must NOT burn the day's remaining
    // boundaries: pre-fix, a both-lanes-disabled cycle resolved instantly
    // and the day loop consumed every boundary up to 15:30 in one tick —
    // re-enabling a lane minutes later could never produce another cycle
    // until the next IST day. The park keeps the boundary horizon intact
    // (level-triggered re-check, missed boundaries counted loud).
    let log = Arc::new(Mutex::new(Vec::new()));
    let exec = Arc::new(RecordingExecutor {
        log: Arc::clone(&log),
        chain_verdict: empty_chain,
        spot_verdict: empty_spot,
    });
    let shutdown = Arc::new(Notify::new());
    let groww_enabled = Arc::new(AtomicBool::new(false));
    let deps = CadenceRunnerDeps {
        config: CadenceConfig::default(),
        calendar: test_calendar(),
        dhan_executor: Arc::clone(&exec),
        groww_executor: Arc::clone(&exec),
        dhan_enabled: Arc::new(AtomicBool::new(false)),
        groww_enabled: Arc::clone(&groww_enabled),
        shutdown: Arc::clone(&shutdown),
    };
    let clock = Arc::new(TestClock {
        anchor: tokio::time::Instant::now(),
        base_wall_ms: BASE_WALL_MS,
        date: NaiveDate::from_ymd_opt(2026, 7, 14).expect("valid date"), // Tuesday
    });
    let task = tokio::spawn(run_cadence_loop(clock, deps));

    // ~3 minutes parked with BOTH lanes disabled (paused time advances
    // through the park polls instantly).
    for _ in 0..180 {
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
    {
        // Nothing may have fired while parked.
        // APPROVED (test-only): poisoned mutex propagates the panic.
        #[allow(clippy::unwrap_used)]
        let calls = log.lock().unwrap().clone();
        assert!(calls.is_empty(), "no fires while both lanes are disabled");
    }
    // Re-enable Groww: the runner must resume at the NEXT joinable
    // boundary (boundaries were NOT consumed while parked).
    groww_enabled.store(true, std::sync::atomic::Ordering::Release);
    for _ in 0..180 {
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
    shutdown.notify_waiters();
    let exit = tokio::time::timeout(std::time::Duration::from_secs(120), task)
        .await
        .expect("runner must exit after shutdown")
        .expect("runner task must not panic");
    assert_eq!(exit, LoopExit::Shutdown);

    // APPROVED (test-only): poisoned mutex propagates the panic.
    #[allow(clippy::unwrap_used)]
    let calls = log.lock().unwrap().clone();
    assert!(
        calls.iter().any(|c| matches!(
            c,
            RecordedCall::Chain {
                feed: Feed::Groww,
                ..
            } | RecordedCall::Spot {
                feed: Feed::Groww,
                ..
            }
        )),
        "the Groww lane must fire again after the re-enable — the day's \
         boundaries were not burned by the disabled park"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_cadence_supervisor_graceful_shutdown_not_respawning() {
    // The SUPERVISOR itself (spawn_supervised_cadence_runner, system
    // clock): a graceful shutdown terminates it without a respawn. Both
    // lanes disabled ⇒ the inner loop parks in a shutdown-responsive
    // select regardless of the real wall-clock instant this test runs at
    // (in-session parks on the disabled gate; off-session parks on the
    // calendar/window gate).
    let log = Arc::new(Mutex::new(Vec::new()));
    let exec = Arc::new(RecordingExecutor {
        log,
        chain_verdict: empty_chain,
        spot_verdict: empty_spot,
    });
    let (deps, shutdown) = deps_with(exec, false, false);
    let handle = spawn_supervised_cadence_runner(deps);
    // Notify repeatedly: Notify carries no pre-registration permit for
    // notify_waiters, so keep signalling until the supervisor observes it.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while !handle.is_finished() {
        assert!(
            std::time::Instant::now() < deadline,
            "the supervisor must exit on graceful shutdown"
        );
        shutdown.notify_waiters();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    handle.await.expect("supervisor task must not panic");
}
