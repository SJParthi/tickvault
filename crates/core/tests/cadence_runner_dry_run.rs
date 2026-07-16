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
    CadenceExecutor, CadenceFetchError, ChainFetchOk, ChainFetchRequest, ExpiryListRequest,
    ExpiryResolver, SpotFetchRequest, SpotSnapshot, SpotTarget, StubExpiryResolver,
};
use tickvault_core::cadence::expiry::DayLockedExpiryStore;
use tickvault_core::cadence::gate::DhanGates;
use tickvault_core::cadence::runner::{
    CadenceClock, CadenceRunnerDeps, LoopExit, emit_expiry_deadline_page, exhausted_edge_step,
    run_cadence_loop, spawn_supervised_cadence_runner,
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

/// One recorded executor call (`at_ms` = paused-tokio elapsed at the
/// dispatch instant — under `start_paused` the runner wakes at EXACT
/// slot targets, so wave/group instants are deterministically
/// assertable: `wall = BASE_WALL_MS + at_ms`).
#[derive(Clone, Debug, PartialEq, Eq)]
enum RecordedCall {
    Chain {
        feed: Feed,
        underlying: ChainUnderlying,
        cycle_minute_ist: u32,
        expiry_yyyymmdd: Option<u32>,
        at_ms: i64,
    },
    Spot {
        feed: Feed,
        target: SpotTarget,
        cycle_minute_ist: u32,
        at_ms: i64,
    },
}

impl RecordedCall {
    fn minute(&self) -> u32 {
        match self {
            Self::Chain {
                cycle_minute_ist, ..
            }
            | Self::Spot {
                cycle_minute_ist, ..
            } => *cycle_minute_ist,
        }
    }

    fn at_ms(&self) -> i64 {
        match self {
            Self::Chain { at_ms, .. } | Self::Spot { at_ms, .. } => *at_ms,
        }
    }
}

/// Scripted recording executor: logs every request; outcome decided by
/// the injected chain/spot verdict fns (call-count-aware for retry
/// scripting).
struct RecordingExecutor {
    log: Arc<Mutex<Vec<RecordedCall>>>,
    start: tokio::time::Instant,
    chain_verdict: fn(&ChainFetchRequest, usize) -> Result<ChainFetchOk, CadenceFetchError>,
    spot_verdict: fn(&SpotFetchRequest, usize) -> Result<SpotSnapshot, CadenceFetchError>,
    /// Scripted expiry-list verdict (Workstream A, 2026-07-15): consulted
    /// only when the deps carry an `expiry_store` (the resolution loop);
    /// the legacy tests pass `expiry_store: None`, so `empty_expiry_list`
    /// is never reached there.
    expiry_verdict: fn(&ExpiryListRequest) -> Result<Vec<u32>, CadenceFetchError>,
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
            // APPROVED: paused-test elapsed fits i64 comfortably.
            #[allow(clippy::cast_possible_truncation)]
            log.push(RecordedCall::Chain {
                feed: req.feed,
                underlying: req.underlying,
                cycle_minute_ist: req.cycle_minute_ist,
                expiry_yyyymmdd: req.expiry_yyyymmdd,
                at_ms: self.start.elapsed().as_millis() as i64,
            });
        }
        let verdict = (self.chain_verdict)(&req, prior);
        async move {
            // Small paused-time latency: same-instant waves/groups all
            // DISPATCH before any completion lands (the real-broker
            // ordering) — instant completions would resolve the lane
            // between two same-instant queue pops.
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            verdict
        }
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
            // APPROVED: paused-test elapsed fits i64 comfortably.
            #[allow(clippy::cast_possible_truncation)]
            log.push(RecordedCall::Spot {
                feed: req.feed,
                target: req.target,
                cycle_minute_ist: req.cycle_minute_ist,
                at_ms: self.start.elapsed().as_millis() as i64,
            });
        }
        let verdict = (self.spot_verdict)(&req, prior);
        async move {
            // See `fetch_chain` — dispatch-before-completion ordering.
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            verdict
        }
    }

    fn fetch_expiry_list(
        &self,
        req: ExpiryListRequest,
    ) -> impl std::future::Future<Output = Result<Vec<u32>, CadenceFetchError>> + Send {
        let verdict = (self.expiry_verdict)(&req);
        async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            verdict
        }
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
    let config = CadenceConfig::default();
    let gates = test_gates(&config);
    let deps = CadenceRunnerDeps {
        config,
        calendar: test_calendar(),
        dhan_executor: Arc::clone(&exec),
        groww_executor: exec,
        dhan_enabled: Arc::new(AtomicBool::new(dhan_on)),
        groww_enabled: Arc::new(AtomicBool::new(groww_on)),
        expiry_resolver: Arc::new(StubExpiryResolver),
        expiry_store: None,
        gates,
        dry_run: false,
        notifier: None,
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

fn empty_expiry_list(_req: &ExpiryListRequest) -> Result<Vec<u32>, CadenceFetchError> {
    Err(CadenceFetchError::Empty)
}

/// Fresh isolated Dhan gates for one test (production shares the
/// process-global registry; tests must never contend across threads).
fn test_gates(cfg: &CadenceConfig) -> Arc<DhanGates> {
    Arc::new(DhanGates::new(
        cfg.chain_min_spacing_ms,
        cfg.spot_window_cap,
    ))
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
        start: tokio::time::Instant::now(),
        chain_verdict: empty_chain,
        spot_verdict: empty_spot,
        expiry_verdict: empty_expiry_list,
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
    // ExpiryResolver seam (2026-07-15): the day-1 stub is UNRESOLVED by
    // design — every chain request is stamped `None` (the scheduler
    // never guesses; the lane's coalesced CADENCE-01 carries the
    // `expiry_unresolved` stage).
    assert!(
        !calls.iter().any(|c| matches!(
            c,
            RecordedCall::Chain {
                expiry_yyyymmdd: Some(_),
                ..
            }
        )),
        "the StubExpiryResolver must stamp every chain request None"
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
        start: tokio::time::Instant::now(),
        chain_verdict,
        spot_verdict,
        expiry_verdict: empty_expiry_list,
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
    emit_decision(&snap, false); // one CADENCE-02-coded emission
    emit_decision(&snap, true); // F10: dry-run demotes to info! (no panic path)
    assert!(
        !latch.try_latch(snap.lane, snap.cycle_minute_ist),
        "a second cutoff/late-completion race can never re-emit"
    );
    if let DecisionOutcome::Skipped(reason) = snap.outcome {
        assert_eq!(reason.as_str(), "cutoff");
    }
}

#[test]
fn test_emit_expiry_deadline_page_dry_run_demotion_both_arms() {
    // RS9 (2026-07-16, the F10 demotion pattern applied to the expiry
    // DEADLINE page arm): under dry-run executors expiry resolution can
    // never succeed (every expiry-list fetch returns Empty), so the
    // ~08:55 IST deadline page would fire 3-6 coded error! lines every
    // dry-run day of pure expected-shape noise. dry_run=false keeps the
    // coded CADENCE-01 error!; dry_run=true demotes to info! with a
    // dry_run=true field (counter unchanged — the trend survives). The
    // F10 test shape: both arms exercised, no panic path, per-arm
    // semantics pinned by the emit fn's branch (source: runner.rs
    // emit_expiry_deadline_page).
    for underlying in ChainUnderlying::ALL {
        emit_expiry_deadline_page(false, Feed::Dhan, *underlying, 32_100); // coded error! arm
        emit_expiry_deadline_page(true, Feed::Groww, *underlying, 32_100); // RS9: info! demotion arm
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
        start: tokio::time::Instant::now(),
        chain_verdict: empty_chain,
        spot_verdict: empty_spot,
        expiry_verdict: empty_expiry_list,
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
        expiry_resolver: Arc::new(StubExpiryResolver),
        expiry_store: None,
        gates: test_gates(&CadenceConfig::default()),
        dry_run: false,
        notifier: None,
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
        start: tokio::time::Instant::now(),
        chain_verdict: empty_chain,
        spot_verdict: empty_spot,
        expiry_verdict: empty_expiry_list,
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

// ---------------------------------------------------------------------------
// 2026-07-15 adaptive ladders + ExpiryResolver seam (runner end-to-end)
// ---------------------------------------------------------------------------

/// Slot-instant tolerance, ms: paused-tokio wakes land at exact targets;
/// a small allowance absorbs completion-processing interleaving.
const SLOT_TOLERANCE_MS: i64 = 50;

/// Expected `at_ms` (executor-elapsed) of a wave/group instant `offset_ms`
/// after the boundary CLOSING `minute` (boundary = minute + 60s).
fn expected_at(minute: u32, offset_ms: i64) -> i64 {
    (i64::from(minute) + 60) * 1_000 - BASE_WALL_MS + offset_ms
}

fn assert_slot(actual: i64, minute: u32, offset_ms: i64, what: &str) {
    let expected = expected_at(minute, offset_ms);
    assert!(
        actual >= expected && actual - expected <= SLOT_TOLERANCE_MS,
        "{what}: fired at {actual}, expected {expected} (+{SLOT_TOLERANCE_MS}ms)"
    );
}

/// Earliest (FIRST-attempt) dispatch instant among a cycle's calls
/// matching `pred` — primaries fire at the wave/group instant; fallback
/// re-fetches come strictly later.
fn first_at(calls: &[RecordedCall], minute: u32, pred: impl Fn(&RecordedCall) -> bool) -> i64 {
    calls
        .iter()
        .filter(|c| c.minute() == minute && pred(c))
        .map(RecordedCall::at_ms)
        .min()
        .expect("the cycle must have fired the leg")
}

#[tokio::test(start_paused = true)]
async fn test_cadence_expiry_resolver_stamps_requests_when_resolved() {
    // The 2026-07-15 ExpiryResolver seam end-to-end: a RESOLVING
    // resolver's yyyymmdd lands on EVERY chain request (both lanes —
    // Dhan primaries/retries + Groww waves/fallback) at request-build
    // time; the None passthrough is pinned by the stub assert in the
    // full-cycle test above.
    struct FixedExpiry;
    impl ExpiryResolver for FixedExpiry {
        fn resolved_expiry(&self, _b: Feed, _u: ChainUnderlying, _d: NaiveDate) -> Option<u32> {
            Some(20_260_730)
        }
    }
    let log = Arc::new(Mutex::new(Vec::new()));
    let exec = Arc::new(RecordingExecutor {
        log: Arc::clone(&log),
        start: tokio::time::Instant::now(),
        chain_verdict: empty_chain,
        spot_verdict: empty_spot,
        expiry_verdict: empty_expiry_list,
    });
    let shutdown = Arc::new(Notify::new());
    let deps = CadenceRunnerDeps {
        config: CadenceConfig::default(),
        calendar: test_calendar(),
        dhan_executor: Arc::clone(&exec),
        groww_executor: exec,
        dhan_enabled: Arc::new(AtomicBool::new(true)),
        groww_enabled: Arc::new(AtomicBool::new(true)),
        expiry_resolver: Arc::new(FixedExpiry),
        expiry_store: None,
        gates: test_gates(&CadenceConfig::default()),
        dry_run: false,
        notifier: None,
        shutdown: Arc::clone(&shutdown),
    };
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

    // APPROVED (test-only): poisoned mutex propagates the panic.
    #[allow(clippy::unwrap_used)]
    let calls = log.lock().unwrap().clone();
    let chains: Vec<_> = calls
        .iter()
        .filter(|c| matches!(c, RecordedCall::Chain { .. }))
        .collect();
    assert!(!chains.is_empty(), "the cycle must have fired chains");
    assert!(
        chains.iter().all(|c| matches!(
            c,
            RecordedCall::Chain {
                expiry_yyyymmdd: Some(20_260_730),
                ..
            }
        )),
        "every chain request must carry the resolver's yyyymmdd stamp"
    );
}

#[tokio::test(start_paused = true)]
async fn test_dhan_spot_ladder_rate_limit_mid_ladder_degrades_then_recovers() {
    // The 2026-07-15 Dhan spot CONCURRENCY ladder end-to-end under the
    // 2026-07-16 all-7 shape, driven by rate limits MID-SPOT-LADDER:
    // cycles 1-2 every spot leg 429s (dirty ×2 consecutive → BOTH the
    // spot tier AND the Dhan shape ladder degrade one step — RateLimited
    // is the SOLE arming class, and per the same-day correction each
    // 429'd leg KEEPS its one bounded in-cycle retry) so cycle 3 runs
    // shape 1 + step 1 (buckets [1,1,1,2]: three spots together in the
    // rung-1 spot second, the 4th one window later; cycle 3 stays dirty
    // so the overflow bucket is observable); cycles 4-6 are fully clean
    // (×3 consecutive → recover) so cycle 7 is back at shape 0 + step 0
    // (the all-7 primary: ALL 4 spots concurrent in the burst second).
    fn chain_ok(_req: &ChainFetchRequest, _p: usize) -> Result<ChainFetchOk, CadenceFetchError> {
        Ok(ChainFetchOk {
            underlying_spot: Some(24_500.0),
            published_to_registry: false,
        })
    }
    fn spot_verdict(req: &SpotFetchRequest, _p: usize) -> Result<SpotSnapshot, CadenceFetchError> {
        if req.cycle_minute_ist < FIRST_CYCLE_MINUTE + 180 {
            return Err(CadenceFetchError::RateLimited {
                retry_after_ms: None,
            });
        }
        Ok(SpotSnapshot {
            price: 24_500.0,
            source_minute_ist: req.cycle_minute_ist,
            received_at_epoch_ms: 0,
        })
    }
    let log = Arc::new(Mutex::new(Vec::new()));
    let exec = Arc::new(RecordingExecutor {
        log: Arc::clone(&log),
        start: tokio::time::Instant::now(),
        chain_verdict: chain_ok,
        spot_verdict,
        expiry_verdict: empty_expiry_list,
    });
    // Dhan-only lane (isolates the spot ladder from the Groww shapes).
    let (deps, shutdown) = deps_with(Arc::clone(&exec), true, false);
    let clock = Arc::new(TestClock {
        anchor: tokio::time::Instant::now(),
        base_wall_ms: BASE_WALL_MS,
        date: NaiveDate::from_ymd_opt(2026, 7, 14).expect("valid date"),
    });
    let task = tokio::spawn(run_cadence_loop(clock, deps));
    for _ in 0..540 {
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
    let spot_instants = |minute: u32| -> Vec<i64> {
        let mut v: Vec<i64> = calls
            .iter()
            .filter(|c| c.minute() == minute && matches!(c, RecordedCall::Spot { .. }))
            .map(RecordedCall::at_ms)
            .collect();
        v.sort_unstable();
        v
    };
    // Cycle 1 (shape 0 = ALL-7, step 0, dirty): 4 nominal spots
    // CONCURRENT in the burst second, plus 4 appended in-cycle retries
    // (Correction 2: a 429'd leg keeps its ONE bounded retry) stepping
    // one 1000ms window each past the last nominal slot.
    let c1 = spot_instants(FIRST_CYCLE_MINUTE);
    assert_eq!(c1.len(), 8, "cycle 1: 4 nominal spots + 4 bounded retries");
    assert!(
        c1[3] - c1[0] <= SLOT_TOLERANCE_MS,
        "shape 0 = all-7: ALL 4 spots in the burst second (spread {})",
        c1[3] - c1[0]
    );
    assert!(
        (c1[4] - c1[0] - 1_000).abs() <= SLOT_TOLERANCE_MS,
        "first appended retry one 1000ms window past the burst (got +{})",
        c1[4] - c1[0]
    );
    assert!(
        (c1[7] - c1[4] - 3_000).abs() <= SLOT_TOLERANCE_MS,
        "retries step one window each (retry spread {})",
        c1[7] - c1[4]
    );
    // Cycle 3 (shape 1 + step 1 after 2 consecutive dirty cycles;
    // itself dirty so the lane never resolves early): buckets
    // [1,1,1,2] — three together in the rung-1 spot second, the 4th
    // one full window later, plus the 4 appended retries.
    let c3 = spot_instants(FIRST_CYCLE_MINUTE + 120);
    assert_eq!(c3.len(), 8, "cycle 3: 4 nominal spots + 4 bounded retries");
    assert!(
        c3[2] - c3[0] <= SLOT_TOLERANCE_MS,
        "shape 1 + step 1: first group of 3"
    );
    assert!(
        (c3[3] - c3[0] - 1_000).abs() <= SLOT_TOLERANCE_MS,
        "step-1 overflow bucket exactly one 1000ms window later (got +{})",
        c3[3] - c3[0]
    );
    assert!(
        (c3[4] - c3[3] - 1_000).abs() <= SLOT_TOLERANCE_MS,
        "first appended retry one window past the overflow bucket (got +{})",
        c3[4] - c3[3]
    );
    // Cycle 7 (shape 0 + step 0 again after 3 consecutive clean cycles
    // 4-6): recovered to the all-7 primary — 4 spots, all burst, no
    // retries (clean).
    let c7 = spot_instants(FIRST_CYCLE_MINUTE + 360);
    assert_eq!(c7.len(), 4, "cycle 7: 4 spot singles (clean, no retries)");
    assert!(
        c7[3] - c7[0] <= SLOT_TOLERANCE_MS,
        "recovered to shape 0 = all-7 burst (spread {})",
        c7[3] - c7[0]
    );
}

#[tokio::test(start_paused = true)]
#[allow(clippy::too_many_lines)]
async fn test_groww_three_choice_ladder_all_transitions_and_vix_waves() {
    // The 2026-07-15 Groww THREE-CHOICE fallback-shape ladder end-to-end
    // — ALL transitions (choice 1→2, 2→3, 3→2, 2→1) plus the VIX wave
    // placement per choice: cycles 1-5 rate-limit every spot leg
    // (dirty) so the ladder walks choice 1→2 (after cycle 2) →3 (after
    // cycle 4), and cycle 5 — dirty — exercises choice 3 with the lane
    // unresolved (a clean choice-3 cycle resolves on the core spots and
    // honestly skips the trailing VIX wave); cycles 6+ are clean so it
    // recovers 3→2 (after cycle 8) →1 (after cycle 11). PARTIAL WAVE
    // FAILURES throughout: chains succeed while spots 429 — the verdict
    // refetches only failures.
    fn chain_ok(_req: &ChainFetchRequest, _p: usize) -> Result<ChainFetchOk, CadenceFetchError> {
        Ok(ChainFetchOk {
            underlying_spot: Some(24_500.0),
            published_to_registry: false,
        })
    }
    fn spot_verdict(req: &SpotFetchRequest, _p: usize) -> Result<SpotSnapshot, CadenceFetchError> {
        if req.cycle_minute_ist < FIRST_CYCLE_MINUTE + 300 {
            return Err(CadenceFetchError::RateLimited {
                retry_after_ms: None,
            });
        }
        Ok(SpotSnapshot {
            price: 24_500.0,
            source_minute_ist: req.cycle_minute_ist,
            received_at_epoch_ms: 0,
        })
    }
    let log = Arc::new(Mutex::new(Vec::new()));
    let exec = Arc::new(RecordingExecutor {
        log: Arc::clone(&log),
        start: tokio::time::Instant::now(),
        chain_verdict: chain_ok,
        spot_verdict,
        expiry_verdict: empty_expiry_list,
    });
    // Groww-only lane (the shape ladder is independent of Dhan's).
    let (deps, shutdown) = deps_with(Arc::clone(&exec), false, true);
    let clock = Arc::new(TestClock {
        anchor: tokio::time::Instant::now(),
        base_wall_ms: BASE_WALL_MS,
        date: NaiveDate::from_ymd_opt(2026, 7, 14).expect("valid date"),
    });
    let task = tokio::spawn(run_cadence_loop(clock, deps));
    for _ in 0..780 {
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
    let is_chain = |c: &RecordedCall| matches!(c, RecordedCall::Chain { .. });
    let is_core_spot = |c: &RecordedCall| matches!(c, RecordedCall::Spot { target, .. } if target.chain_underlying().is_some());
    let is_vix_spot = |c: &RecordedCall| {
        matches!(
            c,
            RecordedCall::Spot {
                target: SpotTarget::IndiaVix,
                ..
            }
        )
    };
    // Cycle 1 — choice 1: ALL 7 in parallel at the :00 anchor.
    let m1 = FIRST_CYCLE_MINUTE;
    assert_slot(first_at(&calls, m1, is_chain), m1, 0, "c1 chains");
    assert_slot(first_at(&calls, m1, is_core_spot), m1, 0, "c1 core spots");
    assert_slot(first_at(&calls, m1, is_vix_spot), m1, 0, "c1 vix");
    // Cycle 3 — choice 2 (degraded after 2 dirty): :01 all 3 chains,
    // :02 ALL 4 spots (VIX INCLUDED — coordinator 2026-07-15).
    let m3 = FIRST_CYCLE_MINUTE + 120;
    assert_slot(first_at(&calls, m3, is_chain), m3, 1_000, "c3 chains");
    assert_slot(
        first_at(&calls, m3, is_core_spot),
        m3,
        2_000,
        "c3 core spots",
    );
    assert_slot(
        first_at(&calls, m3, is_vix_spot),
        m3,
        2_000,
        "c3 vix with spots",
    );
    // Cycle 5 — choice 3 (degraded after 4 dirty): :01 chains, :02 core
    // spots, :03 VIX ALONE (last resort only).
    let m5 = FIRST_CYCLE_MINUTE + 240;
    assert_slot(first_at(&calls, m5, is_chain), m5, 1_000, "c5 chains");
    assert_slot(
        first_at(&calls, m5, is_core_spot),
        m5,
        2_000,
        "c5 core spots",
    );
    assert_slot(first_at(&calls, m5, is_vix_spot), m5, 3_000, "c5 vix alone");
    // Cycle 9 — choice 2 again (recovered after 3 clean cycles 6-8):
    // VIX rejoins the :02 spot wave.
    let m9 = FIRST_CYCLE_MINUTE + 480;
    assert_slot(first_at(&calls, m9, is_chain), m9, 1_000, "c9 chains");
    assert_slot(
        first_at(&calls, m9, is_vix_spot),
        m9,
        2_000,
        "c9 vix with spots",
    );
    // Cycle 12 — choice 1 again (fully recovered): the :00 burst.
    let m12 = FIRST_CYCLE_MINUTE + 660;
    assert_slot(first_at(&calls, m12, is_chain), m12, 0, "c12 chains");
    assert_slot(
        first_at(&calls, m12, is_core_spot),
        m12,
        0,
        "c12 core spots",
    );
    assert_slot(first_at(&calls, m12, is_vix_spot), m12, 0, "c12 vix");
    // No wave (nor its sequential fallback tail) ever bleeds into the
    // NEXT minute's :00 burst: every recorded dispatch for cycle N
    // lands strictly before cycle N+1's boundary anchor.
    for c in &calls {
        let next_anchor = expected_at(c.minute() + 60, 0);
        assert!(
            c.at_ms() < next_anchor,
            "a cycle-{} dispatch at {} overlaps the next :00 burst at {}",
            c.minute(),
            c.at_ms(),
            next_anchor
        );
    }
}

// ---------------------------------------------------------------------------
// Workstream A (2026-07-15): pre-market expiry resolution end-to-end
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn test_cadence_runner_expiry_boot_phase_resolves_and_stamps() {
    // The boot-phase resolution loop end-to-end: both brokers' expiry
    // lists resolve at spawn (bounded retry never needed — first attempt
    // succeeds), the day-locked store records the POLICY dates, and the
    // FIRST cycle's chain requests are stamped per-underlying from the
    // store's winning date — NIFTY/SENSEX = nearest active date;
    // BANKNIFTY = the nearest active month's LAST date (never flat min).
    fn expiry_list(_req: &ExpiryListRequest) -> Result<Vec<u32>, CadenceFetchError> {
        // Unsorted vendor-raw list: weeklies 16/23 July + monthly-last 30
        // July + an August date + garbage. Today = 2026-07-14 ⇒
        // NearestActiveDate = 20260716; LastExpiryOfNearestActiveMonth =
        // 20260730.
        Ok(vec![
            20_260_723, 20_260_716, 99_999_999, 20_260_730, 20_260_806,
        ])
    }
    let log = Arc::new(Mutex::new(Vec::new()));
    let exec = Arc::new(RecordingExecutor {
        log: Arc::clone(&log),
        start: tokio::time::Instant::now(),
        chain_verdict: empty_chain,
        spot_verdict: empty_spot,
        expiry_verdict: expiry_list,
    });
    let shutdown = Arc::new(Notify::new());
    let store = Arc::new(DayLockedExpiryStore::new());
    let config = CadenceConfig::default();
    let gates = test_gates(&config);
    let deps = CadenceRunnerDeps {
        config,
        calendar: test_calendar(),
        dhan_executor: Arc::clone(&exec),
        groww_executor: exec,
        dhan_enabled: Arc::new(AtomicBool::new(true)),
        groww_enabled: Arc::new(AtomicBool::new(true)),
        // The store IS the resolver read facade (production wiring).
        expiry_resolver: Arc::clone(&store) as Arc<dyn ExpiryResolver>,
        expiry_store: Some(Arc::clone(&store)),
        gates,
        dry_run: false,
        notifier: None,
        shutdown: Arc::clone(&shutdown),
    };
    let date = NaiveDate::from_ymd_opt(2026, 7, 14).expect("valid date"); // Tuesday
    let clock = Arc::new(TestClock {
        anchor: tokio::time::Instant::now(),
        base_wall_ms: BASE_WALL_MS,
        date,
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

    // The store is day-locked with the POLICY dates per underlying.
    let nifty = store.view(date, ChainUnderlying::Nifty);
    let banknifty = store.view(date, ChainUnderlying::Banknifty);
    assert_eq!(
        nifty.winner.map(|d| d.yyyymmdd()),
        Some(20_260_716),
        "NIFTY = NearestActiveDate"
    );
    assert_eq!(
        banknifty.winner.map(|d| d.yyyymmdd()),
        Some(20_260_730),
        "BANKNIFTY = the nearest active month's LAST date, never flat min"
    );
    assert!(!nifty.disagreement, "identical broker lists never disagree");
    // Every chain request carries the store's winning stamp.
    // APPROVED (test-only): poisoned mutex propagates the panic.
    #[allow(clippy::unwrap_used)]
    let calls = log.lock().unwrap().clone();
    for c in &calls {
        if let RecordedCall::Chain {
            underlying,
            expiry_yyyymmdd,
            ..
        } = c
        {
            let expected = match underlying {
                ChainUnderlying::Banknifty => 20_260_730,
                ChainUnderlying::Nifty | ChainUnderlying::Sensex => 20_260_716,
            };
            assert_eq!(
                *expiry_yyyymmdd,
                Some(expected),
                "chain request for {underlying:?} must carry the day-locked policy date"
            );
        }
    }
    assert!(
        calls
            .iter()
            .any(|c| matches!(c, RecordedCall::Chain { .. })),
        "the cycle must have fired chains"
    );
}

#[tokio::test(start_paused = true)]
async fn test_cadence_runner_expiry_disagreement_dhan_wins_both_lanes() {
    // The DISAGREEMENT arm (operator spec 2026-07-15): both brokers
    // resolve, dates differ ⇒ Dhan WINS for keying BOTH lanes; the store
    // records both raws + the disagreement verdict (the edge-latched
    // CADENCE-01 `expiry_disagreement` fires once — asserted here via
    // the store's latch, the log side is the tag-guard's domain).
    fn expiry_list(req: &ExpiryListRequest) -> Result<Vec<u32>, CadenceFetchError> {
        match req.broker {
            Feed::Dhan => Ok(vec![20_260_716]),
            Feed::Groww => Ok(vec![20_260_717]),
        }
    }
    let log = Arc::new(Mutex::new(Vec::new()));
    let exec = Arc::new(RecordingExecutor {
        log: Arc::clone(&log),
        start: tokio::time::Instant::now(),
        chain_verdict: empty_chain,
        spot_verdict: empty_spot,
        expiry_verdict: expiry_list,
    });
    let shutdown = Arc::new(Notify::new());
    let store = Arc::new(DayLockedExpiryStore::new());
    let config = CadenceConfig::default();
    let gates = test_gates(&config);
    let deps = CadenceRunnerDeps {
        config,
        calendar: test_calendar(),
        dhan_executor: Arc::clone(&exec),
        groww_executor: exec,
        dhan_enabled: Arc::new(AtomicBool::new(true)),
        groww_enabled: Arc::new(AtomicBool::new(true)),
        expiry_resolver: Arc::clone(&store) as Arc<dyn ExpiryResolver>,
        expiry_store: Some(Arc::clone(&store)),
        gates,
        dry_run: false,
        notifier: None,
        shutdown: Arc::clone(&shutdown),
    };
    let date = NaiveDate::from_ymd_opt(2026, 7, 14).expect("valid date");
    let clock = Arc::new(TestClock {
        anchor: tokio::time::Instant::now(),
        base_wall_ms: BASE_WALL_MS,
        date,
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

    let view = store.view(date, ChainUnderlying::Nifty);
    assert_eq!(view.dhan_raw.map(|d| d.yyyymmdd()), Some(20_260_716));
    assert_eq!(view.groww_raw.map(|d| d.yyyymmdd()), Some(20_260_717));
    assert_eq!(
        view.winner.map(|d| d.yyyymmdd()),
        Some(20_260_716),
        "Dhan WINS the disagreement"
    );
    assert!(view.disagreement, "the disagreement verdict is recorded");
    // BOTH lanes' chain requests are keyed on the DHAN date.
    // APPROVED (test-only): poisoned mutex propagates the panic.
    #[allow(clippy::unwrap_used)]
    let calls = log.lock().unwrap().clone();
    let groww_chains: Vec<_> = calls
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
        .collect();
    assert!(!groww_chains.is_empty(), "Groww chains must have fired");
    assert!(
        groww_chains.iter().all(|c| matches!(
            c,
            RecordedCall::Chain {
                expiry_yyyymmdd: Some(20_260_716),
                ..
            }
        )),
        "the GROWW lane is keyed on the winning DHAN date"
    );
}

// ---------------------------------------------------------------------------
// Verifier F4 (2026-07-15): the verdict never refetches an in-flight leg
// ---------------------------------------------------------------------------

/// A Groww executor whose BANKNIFTY chain leg is SLOW (completes after
/// the ~T+800ms verdict instant, before the lane cutoff) — the F4 probe.
struct SlowLegExecutor {
    log: Arc<Mutex<Vec<RecordedCall>>>,
    start: tokio::time::Instant,
}

impl CadenceExecutor for SlowLegExecutor {
    fn fetch_chain(
        &self,
        req: ChainFetchRequest,
    ) -> impl std::future::Future<Output = Result<ChainFetchOk, CadenceFetchError>> + Send {
        {
            // APPROVED (test-only): poisoned mutex propagates the panic.
            #[allow(clippy::unwrap_used)]
            let mut log = self.log.lock().unwrap();
            // APPROVED: paused-test elapsed fits i64 comfortably.
            #[allow(clippy::cast_possible_truncation)]
            log.push(RecordedCall::Chain {
                feed: req.feed,
                underlying: req.underlying,
                cycle_minute_ist: req.cycle_minute_ist,
                expiry_yyyymmdd: req.expiry_yyyymmdd,
                at_ms: self.start.elapsed().as_millis() as i64,
            });
        }
        let slow = req.underlying == ChainUnderlying::Banknifty;
        async move {
            // The slow leg is still IN FLIGHT at the verdict (~+800ms)
            // and lands Ok well inside the 1500ms per-request timeout
            // (L3 truth-sync, 2026-07-15: the original 3000ms delay
            // exceeded the request timeout, so the "Ok" never landed —
            // it TIMED OUT into an Err, which post-L3 correctly earns a
            // deferred fallback. 1200ms matches this test's stated
            // intent: an Ok-completing in-flight leg, first-write-wins,
            // never refetched).
            let delay = if slow { 1_200 } else { 50 };
            tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
            Ok(ChainFetchOk {
                underlying_spot: Some(24_500.0),
                published_to_registry: false,
            })
        }
    }

    fn fetch_spot(
        &self,
        req: SpotFetchRequest,
    ) -> impl std::future::Future<Output = Result<SpotSnapshot, CadenceFetchError>> + Send {
        {
            // APPROVED (test-only): poisoned mutex propagates the panic.
            #[allow(clippy::unwrap_used)]
            let mut log = self.log.lock().unwrap();
            // APPROVED: paused-test elapsed fits i64 comfortably.
            #[allow(clippy::cast_possible_truncation)]
            log.push(RecordedCall::Spot {
                feed: req.feed,
                target: req.target,
                cycle_minute_ist: req.cycle_minute_ist,
                at_ms: self.start.elapsed().as_millis() as i64,
            });
        }
        let minute = req.cycle_minute_ist;
        async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            Ok(SpotSnapshot {
                price: 24_500.0,
                source_minute_ist: minute,
                received_at_epoch_ms: 0,
            })
        }
    }

    async fn fetch_expiry_list(
        &self,
        _req: ExpiryListRequest,
    ) -> Result<Vec<u32>, CadenceFetchError> {
        Err(CadenceFetchError::Empty)
    }
}

#[tokio::test(start_paused = true)]
async fn test_groww_verdict_skips_inflight_leg_never_duplicates() {
    // Verifier F4 (2026-07-15): a leg whose ORIGINAL request is still in
    // flight at the ~T+800ms verdict must NOT be refetched — the pre-fix
    // "Err OR still pending" read fired a duplicate concurrent BANKNIFTY
    // request here (count 2); the fix skips it (count stays 1) and the
    // slow original still lands first-write-wins inside the cutoff.
    let log = Arc::new(Mutex::new(Vec::new()));
    let exec = Arc::new(SlowLegExecutor {
        log: Arc::clone(&log),
        start: tokio::time::Instant::now(),
    });
    let shutdown = Arc::new(Notify::new());
    let config = CadenceConfig::default();
    let gates = test_gates(&config);
    let deps = CadenceRunnerDeps {
        config,
        calendar: test_calendar(),
        dhan_executor: Arc::clone(&exec),
        groww_executor: exec,
        // Groww-only lane (isolates the burst/verdict semantics).
        dhan_enabled: Arc::new(AtomicBool::new(false)),
        groww_enabled: Arc::new(AtomicBool::new(true)),
        expiry_resolver: Arc::new(StubExpiryResolver),
        expiry_store: None,
        gates,
        dry_run: false,
        notifier: None,
        shutdown: Arc::clone(&shutdown),
    };
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

    // APPROVED (test-only): poisoned mutex propagates the panic.
    #[allow(clippy::unwrap_used)]
    let calls = log.lock().unwrap().clone();
    let banknifty_first_cycle = calls
        .iter()
        .filter(|c| {
            c.minute() == FIRST_CYCLE_MINUTE
                && matches!(
                    c,
                    RecordedCall::Chain {
                        feed: Feed::Groww,
                        underlying: ChainUnderlying::Banknifty,
                        ..
                    }
                )
        })
        .count();
    assert_eq!(
        banknifty_first_cycle, 1,
        "the in-flight BANKNIFTY leg is NEVER refetched by the verdict \
         (F4: no duplicate concurrent same-leg request)"
    );
}

// ---------------------------------------------------------------------------
// Verifier L3 (2026-07-15): the DEFERRED per-leg fallback — a leg skipped
// in flight at the verdict that later completes Err still gets its ONE
// fallback attempt
// ---------------------------------------------------------------------------

/// A Groww executor whose BANKNIFTY chain leg FAILS SLOWLY on attempt 1
/// (the Err lands ~T+1200ms — after the ~T+800ms verdict, inside the
/// 1500ms per-request timeout, ~4.8s of room before the 6000ms lane
/// cutoff) and succeeds fast on attempt 2 — the L3 probe.
struct SlowFailLegExecutor {
    log: Arc<Mutex<Vec<RecordedCall>>>,
    start: tokio::time::Instant,
}

impl CadenceExecutor for SlowFailLegExecutor {
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
            // APPROVED: paused-test elapsed fits i64 comfortably.
            #[allow(clippy::cast_possible_truncation)]
            log.push(RecordedCall::Chain {
                feed: req.feed,
                underlying: req.underlying,
                cycle_minute_ist: req.cycle_minute_ist,
                expiry_yyyymmdd: req.expiry_yyyymmdd,
                at_ms: self.start.elapsed().as_millis() as i64,
            });
        }
        let slow_fail = req.underlying == ChainUnderlying::Banknifty && prior == 0;
        async move {
            if slow_fail {
                // Still IN FLIGHT at the ~+800ms verdict; fails at
                // ~+1200ms — inside the 1500ms request timeout.
                tokio::time::sleep(std::time::Duration::from_millis(1_200)).await;
                Err(CadenceFetchError::Transport)
            } else {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                Ok(ChainFetchOk {
                    underlying_spot: Some(24_500.0),
                    published_to_registry: false,
                })
            }
        }
    }

    fn fetch_spot(
        &self,
        req: SpotFetchRequest,
    ) -> impl std::future::Future<Output = Result<SpotSnapshot, CadenceFetchError>> + Send {
        {
            // APPROVED (test-only): poisoned mutex propagates the panic.
            #[allow(clippy::unwrap_used)]
            let mut log = self.log.lock().unwrap();
            // APPROVED: paused-test elapsed fits i64 comfortably.
            #[allow(clippy::cast_possible_truncation)]
            log.push(RecordedCall::Spot {
                feed: req.feed,
                target: req.target,
                cycle_minute_ist: req.cycle_minute_ist,
                at_ms: self.start.elapsed().as_millis() as i64,
            });
        }
        let minute = req.cycle_minute_ist;
        async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            Ok(SpotSnapshot {
                price: 24_500.0,
                source_minute_ist: minute,
                received_at_epoch_ms: 0,
            })
        }
    }

    async fn fetch_expiry_list(
        &self,
        _req: ExpiryListRequest,
    ) -> Result<Vec<u32>, CadenceFetchError> {
        Err(CadenceFetchError::Empty)
    }
}

#[tokio::test(start_paused = true)]
async fn test_groww_deferred_fallback_refetches_inflight_skipped_leg_once() {
    // Verifier L3 (2026-07-15): a leg still IN FLIGHT at the ~T+800ms
    // verdict is SKIPPED (F4 — never a duplicate concurrent request).
    // Pre-fix, when that leg later completed Err (~T+1200 here) there was
    // NO later verdict — terminal on attempt 1, ZERO retries, despite
    // ~4.8s of room inside the 6000ms Groww cutoff (the structurally-DEAD
    // fallback for slow-FAILURE legs). Post-fix the DEFERRED per-leg
    // fallback dispatches its ONE fallback attempt the instant the Err
    // completion lands: EXACTLY 2 BANKNIFTY chain calls in the first
    // cycle — never 1 (dead fallback) and never 3+ (no duplicates; the
    // F4 no-duplicate test stays green beside this).
    let log = Arc::new(Mutex::new(Vec::new()));
    let exec = Arc::new(SlowFailLegExecutor {
        log: Arc::clone(&log),
        start: tokio::time::Instant::now(),
    });
    let shutdown = Arc::new(Notify::new());
    let config = CadenceConfig::default();
    let gates = test_gates(&config);
    let deps = CadenceRunnerDeps {
        config,
        calendar: test_calendar(),
        dhan_executor: Arc::clone(&exec),
        groww_executor: exec,
        // Groww-only lane (isolates the burst/verdict semantics).
        dhan_enabled: Arc::new(AtomicBool::new(false)),
        groww_enabled: Arc::new(AtomicBool::new(true)),
        expiry_resolver: Arc::new(StubExpiryResolver),
        expiry_store: None,
        gates,
        dry_run: false,
        notifier: None,
        shutdown: Arc::clone(&shutdown),
    };
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

    // APPROVED (test-only): poisoned mutex propagates the panic.
    #[allow(clippy::unwrap_used)]
    let calls = log.lock().unwrap().clone();
    let banknifty_first_cycle = calls
        .iter()
        .filter(|c| {
            c.minute() == FIRST_CYCLE_MINUTE
                && matches!(
                    c,
                    RecordedCall::Chain {
                        feed: Feed::Groww,
                        underlying: ChainUnderlying::Banknifty,
                        ..
                    }
                )
        })
        .count();
    assert_eq!(
        banknifty_first_cycle, 2,
        "an in-flight-skipped leg completing Err inside the cutoff budget \
         must get EXACTLY its one deferred fallback attempt (L3): 1 = the \
         dead-fallback bug; 3+ = a duplicate"
    );
}
// ---------------------------------------------------------------------------
// CAD-CORR-1 (hostile round 1, 2026-07-15): a completion processed after a
// suspend across IST midnight must ABANDON the cycle — never be folded
// into the dead day's assembly / deferred-fallback / decision paths
// ---------------------------------------------------------------------------

/// A clock that crosses IST midnight at a scripted elapsed instant: the
/// ms-of-day domain WRAPS to ~0 and the calendar date advances — the
/// exact state a process suspend across midnight resumes into. The
/// monotonic + epoch domains never wrap (they are suspend-immune).
struct MidnightFlipClock {
    anchor: tokio::time::Instant,
    base_wall_ms: i64,
    base_date: NaiveDate,
    flip_at_elapsed_ms: i64,
}

impl MidnightFlipClock {
    fn elapsed_ms(&self) -> i64 {
        // APPROVED: paused-test elapsed fits i64 comfortably.
        #[allow(clippy::cast_possible_truncation)]
        {
            self.anchor.elapsed().as_millis() as i64
        }
    }
}

impl CadenceClock for MidnightFlipClock {
    fn ist_ms_of_day(&self) -> i64 {
        let e = self.elapsed_ms();
        if e < self.flip_at_elapsed_ms {
            self.base_wall_ms + e
        } else {
            // Post-midnight: ms-of-day wrapped to just past 00:00.
            e - self.flip_at_elapsed_ms
        }
    }

    fn ist_date(&self) -> NaiveDate {
        if self.elapsed_ms() < self.flip_at_elapsed_ms {
            self.base_date
        } else {
            self.base_date.succ_opt().expect("next day exists")
        }
    }

    fn monotonic_ms(&self) -> i64 {
        self.elapsed_ms()
    }

    fn epoch_ms(&self) -> i64 {
        self.base_wall_ms + self.elapsed_ms()
    }
}

#[tokio::test(start_paused = true)]
async fn test_completion_after_midnight_suspend_abandons_cycle() {
    // CAD-CORR-1: the biased select drains completions BEFORE the timer
    // arm, so the IST-date-change abandon must exist on the COMPLETION
    // arm too. Script: the BANKNIFTY burst leg is still in flight at the
    // ~T+800ms verdict (skipped, F4); IST midnight flips at T+1000
    // (elapsed 11_000); the leg's Err completion lands at ~T+1200 —
    // processed FIRST by the biased select, BEFORE any timer wake (next
    // chunk wake ~T+5800). Pre-fix, the wrapped ms-of-day (~200) passed
    // the cutoff guards and the L3 deferred fallback dispatched a SECOND
    // BANKNIFTY request against the dead day's cycle; post-fix the
    // completion arm abandons and the count stays 1.
    let log = Arc::new(Mutex::new(Vec::new()));
    let exec = Arc::new(SlowFailLegExecutor {
        log: Arc::clone(&log),
        start: tokio::time::Instant::now(),
    });
    let shutdown = Arc::new(Notify::new());
    let config = CadenceConfig::default();
    let gates = test_gates(&config);
    let deps = CadenceRunnerDeps {
        config,
        calendar: test_calendar(),
        dhan_executor: Arc::clone(&exec),
        groww_executor: exec,
        // Groww-only lane (isolates the burst/verdict/fallback path).
        dhan_enabled: Arc::new(AtomicBool::new(false)),
        groww_enabled: Arc::new(AtomicBool::new(true)),
        expiry_resolver: Arc::new(StubExpiryResolver),
        expiry_store: None,
        gates,
        dry_run: false,
        notifier: None,
        shutdown: Arc::clone(&shutdown),
    };
    let clock = Arc::new(MidnightFlipClock {
        anchor: tokio::time::Instant::now(),
        base_wall_ms: BASE_WALL_MS,
        base_date: NaiveDate::from_ymd_opt(2026, 7, 14).expect("valid date"),
        // Groww burst fires at elapsed 10_000 (wall 09:16:00); midnight
        // flips at 11_000 — after the ~10_800 verdict, before the
        // ~11_200 slow-leg Err completion and before the next timer
        // wake (~15_800).
        flip_at_elapsed_ms: 11_000,
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

    // APPROVED (test-only): poisoned mutex propagates the panic.
    #[allow(clippy::unwrap_used)]
    let calls = log.lock().unwrap().clone();
    let banknifty_first_cycle = calls
        .iter()
        .filter(|c| {
            c.minute() == FIRST_CYCLE_MINUTE
                && matches!(
                    c,
                    RecordedCall::Chain {
                        feed: Feed::Groww,
                        underlying: ChainUnderlying::Banknifty,
                        ..
                    }
                )
        })
        .count();
    assert_eq!(
        banknifty_first_cycle, 1,
        "a completion resumed AFTER a suspend across IST midnight must \
         abandon the cycle (CAD-CORR-1): 2 = the dead-day deferred \
         fallback fired from the completion arm against the wrapped clock"
    );
}

// ---------------------------------------------------------------------------
// CONC-NEW-1 (hostile round 1, 2026-07-15): the runtime lane toggles are
// re-observed while the cycle is pristine AND at every dispatch instant —
// never frozen at run_cycle entry
// ---------------------------------------------------------------------------

/// Entry wall clock ~2 minutes BEFORE the first anchor: run_cycle is
/// entered immediately (the boundary is joinable) and idles in wake
/// chunks — the window where the entry snapshot used to freeze the
/// `/api/feeds` toggles.
const PRE_ANCHOR_WALL_MS: i64 = (9 * 3600 + 14 * 60) * 1_000;

#[tokio::test(start_paused = true)]
async fn test_pristine_cycle_observes_disable_before_first_fire() {
    // Disable the Dhan lane ~10s after run_cycle entry, ~110s BEFORE its
    // first fire (the 09:16:00 Groww burst / 09:16:01 Dhan burst).
    // Pre-fix the entry snapshot
    // fired the full Dhan cycle anyway; post-fix the pristine re-arm
    // drops the lane within one ~5s wake chunk — ZERO Dhan requests.
    let log = Arc::new(Mutex::new(Vec::new()));
    let exec = Arc::new(RecordingExecutor {
        log: Arc::clone(&log),
        start: tokio::time::Instant::now(),
        chain_verdict: empty_chain,
        spot_verdict: empty_spot,
        expiry_verdict: empty_expiry_list,
    });
    let shutdown = Arc::new(Notify::new());
    let config = CadenceConfig::default();
    let gates = test_gates(&config);
    let dhan_enabled = Arc::new(AtomicBool::new(true));
    let deps = CadenceRunnerDeps {
        config,
        calendar: test_calendar(),
        dhan_executor: Arc::clone(&exec),
        groww_executor: exec,
        dhan_enabled: Arc::clone(&dhan_enabled),
        groww_enabled: Arc::new(AtomicBool::new(true)),
        expiry_resolver: Arc::new(StubExpiryResolver),
        expiry_store: None,
        gates,
        dry_run: false,
        notifier: None,
        shutdown: Arc::clone(&shutdown),
    };
    let clock = Arc::new(TestClock {
        anchor: tokio::time::Instant::now(),
        base_wall_ms: PRE_ANCHOR_WALL_MS,
        date: NaiveDate::from_ymd_opt(2026, 7, 14).expect("valid date"),
    });
    let task = tokio::spawn(run_cadence_loop(clock, deps));
    // ~10s in (pristine — the first fire is at +120s), the operator
    // disables the Dhan lane.
    for _ in 0..10 {
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
    dhan_enabled.store(false, std::sync::atomic::Ordering::Release);
    // Let the whole first cycle elapse (cutoff at +135s) with margin.
    for _ in 0..140 {
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
    let dhan_calls = calls
        .iter()
        .filter(|c| {
            matches!(
                c,
                RecordedCall::Chain {
                    feed: Feed::Dhan,
                    ..
                } | RecordedCall::Spot {
                    feed: Feed::Dhan,
                    ..
                }
            )
        })
        .count();
    assert_eq!(
        dhan_calls, 0,
        "a lane disabled BEFORE its first fire must fire NOTHING \
         (CONC-NEW-1: the entry snapshot must not freeze the toggle)"
    );
    assert!(
        calls.iter().any(|c| matches!(
            c,
            RecordedCall::Chain {
                feed: Feed::Groww,
                ..
            }
        )),
        "the Groww lane must be unaffected by the Dhan disable"
    );
}

#[tokio::test(start_paused = true)]
async fn test_pristine_cycle_observes_enable_before_first_fire() {
    // The mirror direction: the Dhan lane starts DISABLED at cycle entry
    // and the operator enables it ~10s in, ~105s before the first fire.
    // Pre-fix the disabled entry snapshot silently missed the whole
    // first cycle; post-fix the pristine re-arm rebuilds the event list
    // and the lane fires its 3 chain primaries in cycle 1.
    let log = Arc::new(Mutex::new(Vec::new()));
    let exec = Arc::new(RecordingExecutor {
        log: Arc::clone(&log),
        start: tokio::time::Instant::now(),
        chain_verdict: empty_chain,
        spot_verdict: empty_spot,
        expiry_verdict: empty_expiry_list,
    });
    let shutdown = Arc::new(Notify::new());
    let config = CadenceConfig::default();
    let gates = test_gates(&config);
    let dhan_enabled = Arc::new(AtomicBool::new(false));
    let deps = CadenceRunnerDeps {
        config,
        calendar: test_calendar(),
        dhan_executor: Arc::clone(&exec),
        groww_executor: exec,
        dhan_enabled: Arc::clone(&dhan_enabled),
        groww_enabled: Arc::new(AtomicBool::new(true)),
        expiry_resolver: Arc::new(StubExpiryResolver),
        expiry_store: None,
        gates,
        dry_run: false,
        notifier: None,
        shutdown: Arc::clone(&shutdown),
    };
    let clock = Arc::new(TestClock {
        anchor: tokio::time::Instant::now(),
        base_wall_ms: PRE_ANCHOR_WALL_MS,
        date: NaiveDate::from_ymd_opt(2026, 7, 14).expect("valid date"),
    });
    let task = tokio::spawn(run_cadence_loop(clock, deps));
    for _ in 0..10 {
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
    dhan_enabled.store(true, std::sync::atomic::Ordering::Release);
    for _ in 0..140 {
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
    let dhan_chains_cycle1 = calls
        .iter()
        .filter(|c| {
            c.minute() == FIRST_CYCLE_MINUTE
                && matches!(
                    c,
                    RecordedCall::Chain {
                        feed: Feed::Dhan,
                        ..
                    }
                )
        })
        .count();
    assert!(
        dhan_chains_cycle1 >= 3,
        "a lane enabled BEFORE its first fire must join the CURRENT cycle \
         (CONC-NEW-1 pristine re-arm): got {dhan_chains_cycle1} Dhan chain \
         calls in cycle 1 (0 = the frozen disabled snapshot)"
    );
}

#[tokio::test(start_paused = true)]
async fn test_midcycle_disable_stops_not_yet_dispatched_fires() {
    // Dispatch-time re-read: with the ALL-7 burst second already
    // dispatched (3 chain primaries + all 4 spots), a disable at wall
    // T+1.5s must stop every LATER fire — the appended per-leg spot
    // retries (Empty is retryable under the 2026-07-16 bounded-retry
    // policy) and the chain retry slot — within the same cycle.
    // Pre-fix the whole cycle kept firing (8 spot requests from a
    // disabled lane).
    let log = Arc::new(Mutex::new(Vec::new()));
    let exec = Arc::new(RecordingExecutor {
        log: Arc::clone(&log),
        start: tokio::time::Instant::now(),
        chain_verdict: empty_chain,
        spot_verdict: empty_spot,
        expiry_verdict: empty_expiry_list,
    });
    let shutdown = Arc::new(Notify::new());
    let config = CadenceConfig::default();
    let gates = test_gates(&config);
    let dhan_enabled = Arc::new(AtomicBool::new(true));
    let deps = CadenceRunnerDeps {
        config,
        calendar: test_calendar(),
        dhan_executor: Arc::clone(&exec),
        groww_executor: exec,
        dhan_enabled: Arc::clone(&dhan_enabled),
        groww_enabled: Arc::new(AtomicBool::new(true)),
        expiry_resolver: Arc::new(StubExpiryResolver),
        expiry_store: None,
        gates,
        dry_run: false,
        notifier: None,
        shutdown: Arc::clone(&shutdown),
    };
    let clock = Arc::new(TestClock {
        anchor: tokio::time::Instant::now(),
        base_wall_ms: BASE_WALL_MS,
        date: NaiveDate::from_ymd_opt(2026, 7, 14).expect("valid date"),
    });
    let task = tokio::spawn(run_cadence_loop(clock, deps));
    // The ALL-7 burst second fires at elapsed 11_000 (wall T+1s:
    // 3 chains + all 4 spots); the appended spot retries start at
    // 12_000 (T+2s) and the chain retry slot sits at 14_000 (T+4s).
    // Disable at 11_500 — after the burst second, before any retry.
    for _ in 0..11 {
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    dhan_enabled.store(false, std::sync::atomic::Ordering::Release);
    for _ in 0..20 {
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
    let dhan_spots = calls
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
    let dhan_chains = calls
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
    assert_eq!(
        dhan_chains, 3,
        "the 3 chain primaries dispatched BEFORE the disable must have \
         fired; the T+4s chain retry slot must have been stopped"
    );
    assert_eq!(
        dhan_spots, 4,
        "a mid-cycle disable must stop every not-yet-dispatched fire \
         (CONC-NEW-1 dispatch-time re-read): only the 4 all-7 \
         burst-second spots fired; 8 = the frozen entry snapshot firing \
         the appended retries from a disabled lane"
    );
    assert!(
        calls.iter().any(|c| matches!(
            c,
            RecordedCall::Chain {
                feed: Feed::Groww,
                ..
            }
        )),
        "the Groww lane must be unaffected by the Dhan disable"
    );
}

#[test]
fn test_exhausted_edge_step_fires_once_per_episode_and_rearms_on_clean() {
    // RS12 (verifier round 5, 2026-07-16): the CADENCE-01
    // `ladder_exhausted` edge-latch had zero test coverage. The pure
    // step (runner.rs `exhausted_edge_step`) is walked through one full
    // scenario: the SHIFT cycle never fires the edge, the first
    // dirty-at-max cycle fires exactly once, the latch holds across
    // sustained dirty cycles, a clean cycle re-arms, and the next
    // dirty-at-max episode fires again.
    //
    // Shift cycle: dirty, but the shape ladder was NOT yet at max BEFORE
    // this cycle's streak advance (this is the cycle whose dirty verdict
    // CAUSES the demotion) — no fire, latch stays off (the "no
    // double-fire across a shift cycle" half of the pin).
    let (fire, ep) = exhausted_edge_step(false, true, false);
    assert!(!fire, "the shift cycle itself must never fire the edge");
    assert!(!ep, "the shift cycle must not latch an episode");
    // First dirty cycle AT the max rung: the rising edge — fires ONCE.
    let (fire, ep) = exhausted_edge_step(ep, true, true);
    assert!(fire, "first dirty-at-max cycle fires the edge");
    assert!(ep, "the episode latches on the rising edge");
    // Sustained dirty at max: latched — never a second fire in-episode.
    let (fire, ep) = exhausted_edge_step(ep, true, true);
    assert!(!fire, "a latched episode never re-fires on sustained dirty");
    assert!(ep, "the latch holds while the episode persists");
    // A CLEAN cycle re-arms, even while the ladder still sits at max.
    let (fire, ep) = exhausted_edge_step(ep, false, true);
    assert!(!fire, "a clean cycle fires nothing");
    assert!(!ep, "a clean cycle re-arms the episode latch");
    // The next dirty-at-max is a NEW episode: the edge fires again.
    let (fire, ep) = exhausted_edge_step(ep, true, true);
    assert!(fire, "a fresh episode fires the edge again");
    assert!(ep, "the new episode latches");
    // Totality arm: dirty but NOT at max mid-episode holds the latch
    // silently (conservative — a fire here would be a phantom edge).
    let (fire, ep) = exhausted_edge_step(ep, true, false);
    assert!(!fire, "dirty off-max never fires");
    assert!(ep, "dirty off-max holds the latch unchanged");
}
