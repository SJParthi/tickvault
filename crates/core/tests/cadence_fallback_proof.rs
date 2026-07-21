//! Fallback-proof runner tests (RUNNER level, paused tokio time):
//! the RateLimited-ONLY arming law of the 2026-07-16 operator correction
//! ("one and only when you tried that multiple times and gets rate
//! limited alone alone fallback") proven end-to-end through
//! `run_cadence_loop` with a scripted recording executor.
//!
//! COVERAGE INVENTORY (what already exists vs what THIS file adds):
//! - (i) Dhan rung 0 → 2 consecutive RateLimited-dirty cycles → rung 1,
//!   and (ii) 3 clean cycles → back to rung 0: ALREADY covered at runner
//!   level by `cadence_runner_dry_run.rs::
//!   test_dhan_spot_ladder_rate_limit_mid_ladder_degrades_then_recovers`.
//! - (iv) Groww lane demote/recover across all three choices: ALREADY
//!   covered by `cadence_runner_dry_run.rs::
//!   test_groww_three_choice_ladder_all_transitions_and_vix_waves`.
//! - (iii) sustained Timeout-only / Transport-only / Empty-only cycles
//!   NEVER demote either lane's shape: covered HERE (the ladder-level
//!   `failure_arms_ladder` negatives exist in `ladder.rs`; this file
//!   proves the same law through the real loop — the observable is fire
//!   TIMING: shape 0 keeps ALL 4 Dhan spots concurrent in the burst
//!   second every cycle, and Groww choice 1 keeps all 7 requests
//!   concurrent at T+0, no matter how many consecutive non-429 dirty
//!   cycles accumulate).
//!
//! Harness pieces (TestClock / RecordingExecutor / deps) are replicated
//! from `cadence_runner_dry_run.rs` — its helpers are file-private by
//! design (integration tests share nothing), so the minimal subset lives
//! here verbatim-in-spirit.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;

use chrono::NaiveDate;
use tickvault_common::config::{CadenceConfig, TradingConfig};
use tickvault_common::trading_calendar::TradingCalendar;
use tickvault_core::cadence::executor::{
    CadenceExecutor, CadenceFetchError, ChainFetchOk, ChainFetchRequest, ExpiryListRequest,
    SpotFetchRequest, SpotSnapshot, StubExpiryResolver,
};
use tickvault_core::cadence::gate::DhanGates;
use tickvault_core::cadence::runner::{
    CadenceClock, CadenceRunnerDeps, LoopExit, run_cadence_loop,
};
use tokio::sync::Notify;

// ---------------------------------------------------------------------------
// Minimal paused-time harness (see the module header for provenance)
// ---------------------------------------------------------------------------

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

/// One recorded call: kind (chain/spot) + the cycle minute + the
/// paused-tokio dispatch instant.
#[derive(Clone, Debug)]
struct RecordedCall {
    is_spot: bool,
    cycle_minute_ist: u32,
    at_ms: i64,
}

/// Scripted recording executor: every SPOT fire fails with the injected
/// error class; chains succeed (isolating the spot-driven dirty cycles,
/// the `test_dhan_spot_ladder_*` precedent) — or, for the Groww variant,
/// BOTH legs fail with the class (a fully dirty cycle either way).
struct FailingExecutor {
    log: Arc<Mutex<Vec<RecordedCall>>>,
    start: tokio::time::Instant,
    spot_err: fn() -> CadenceFetchError,
    chain_err: Option<fn() -> CadenceFetchError>,
}

impl FailingExecutor {
    fn record(&self, is_spot: bool, cycle_minute_ist: u32) {
        // APPROVED (test-only): poisoned mutex propagates the panic.
        #[allow(clippy::unwrap_used)]
        let mut log = self.log.lock().unwrap();
        // APPROVED: paused-test elapsed fits i64 comfortably.
        #[allow(clippy::cast_possible_truncation)]
        log.push(RecordedCall {
            is_spot,
            cycle_minute_ist,
            at_ms: self.start.elapsed().as_millis() as i64,
        });
    }
}

impl CadenceExecutor for FailingExecutor {
    fn fetch_chain(
        &self,
        req: ChainFetchRequest,
    ) -> impl std::future::Future<Output = Result<ChainFetchOk, CadenceFetchError>> + Send {
        self.record(false, req.cycle_minute_ist);
        let verdict = match self.chain_err {
            Some(err) => Err(err()),
            None => Ok(ChainFetchOk {
                underlying_spot: Some(24_500.0),
                published_to_registry: false,
            }),
        };
        async move {
            // Dispatch-before-completion ordering (the dry_run harness
            // precedent): same-instant waves all dispatch first.
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            verdict
        }
    }

    fn fetch_spot(
        &self,
        req: SpotFetchRequest,
    ) -> impl std::future::Future<Output = Result<SpotSnapshot, CadenceFetchError>> + Send {
        self.record(true, req.cycle_minute_ist);
        let verdict = Err((self.spot_err)());
        async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            verdict
        }
    }

    fn fetch_expiry_list(
        &self,
        _req: ExpiryListRequest,
    ) -> impl std::future::Future<Output = Result<Vec<u32>, CadenceFetchError>> + Send {
        async move { Err(CadenceFetchError::Empty) }
    }
}

/// 09:15:50 IST — 10s before the first joinable boundary (09:16:00).
const BASE_WALL_MS: i64 = (9 * 3600 + 15 * 60 + 50) * 1_000;
/// The first cycle's decided minute (09:15:00, seconds-of-day).
const FIRST_CYCLE_MINUTE: u32 = 9 * 3600 + 15 * 60;
const SLOT_TOLERANCE_MS: i64 = 50;

fn deps_with(
    exec: Arc<FailingExecutor>,
    dhan_on: bool,
    groww_on: bool,
) -> (
    CadenceRunnerDeps<FailingExecutor, FailingExecutor>,
    Arc<Notify>,
) {
    let shutdown = Arc::new(Notify::new());
    // This suite pins the kill-switch-OFF legacy shape (B1 defines OFF as
    // byte-equivalent pre-ladder behavior).
    let config = CadenceConfig {
        native_retry_enabled: false,
        ..CadenceConfig::default()
    };
    let gates = Arc::new(DhanGates::new(
        config.chain_min_spacing_ms,
        config.spot_window_cap,
    ));
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

/// Drive a lane for `cycles` full minutes and return the recorded calls.
async fn run_lane(
    spot_err: fn() -> CadenceFetchError,
    chain_err: Option<fn() -> CadenceFetchError>,
    dhan_on: bool,
    groww_on: bool,
    cycles: u32,
) -> Vec<RecordedCall> {
    let log = Arc::new(Mutex::new(Vec::new()));
    let exec = Arc::new(FailingExecutor {
        log: Arc::clone(&log),
        start: tokio::time::Instant::now(),
        spot_err,
        chain_err,
    });
    let (deps, shutdown) = deps_with(Arc::clone(&exec), dhan_on, groww_on);
    let clock = Arc::new(TestClock {
        anchor: tokio::time::Instant::now(),
        base_wall_ms: BASE_WALL_MS,
        date: NaiveDate::from_ymd_opt(2026, 7, 14).expect("valid date"),
    });
    let task = tokio::spawn(run_cadence_loop(clock, deps));
    // 10s to the first boundary + `cycles` minutes + slack.
    for _ in 0..(10 + 60 * cycles + 30) {
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
    calls
}

fn spot_instants(calls: &[RecordedCall], minute: u32) -> Vec<i64> {
    let mut v: Vec<i64> = calls
        .iter()
        .filter(|c| c.is_spot && c.cycle_minute_ist == minute)
        .map(|c| c.at_ms)
        .collect();
    v.sort_unstable();
    v
}

fn all_instants(calls: &[RecordedCall], minute: u32) -> Vec<i64> {
    let mut v: Vec<i64> = calls
        .iter()
        .filter(|c| c.cycle_minute_ist == minute)
        .map(|c| c.at_ms)
        .collect();
    v.sort_unstable();
    v
}

/// (iii) core assertion for the Dhan lane: across EVERY observed cycle
/// the 4 NOMINAL spot fires stay CONCURRENT in the burst second (the
/// shape-0 / step-0 signature) — a demotion would split them (the
/// [1,1,1,2] rung-1 grouping the RateLimited test observes at cycle 3).
/// Each failed leg keeps its ONE bounded in-cycle retry (all classes
/// share the retry budget — `may_retry_in_cycle`), so a dirty cycle
/// records 8 spot fires: nominal 4 burst-concurrent + 4 appended.
fn assert_dhan_shape0_every_cycle(calls: &[RecordedCall], cycles: u32, class: &str) {
    for n in 0..cycles {
        let minute = FIRST_CYCLE_MINUTE + 60 * n;
        let inst = spot_instants(calls, minute);
        assert_eq!(
            inst.len(),
            8,
            "{class}: cycle {} must record 4 nominal spots + 4 bounded \
             in-cycle retries (got {})",
            n + 1,
            inst.len()
        );
        assert!(
            inst[3] - inst[0] <= SLOT_TOLERANCE_MS,
            "{class}: cycle {} nominal spots must stay burst-concurrent \
             (shape 0 held — {class} is NON-ARMING); spread {}ms",
            n + 1,
            inst[3] - inst[0]
        );
    }
}

// ---------------------------------------------------------------------------
// (iii) Dhan lane: sustained non-429 failures NEVER demote the shape
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn test_dhan_sustained_timeout_only_cycles_never_demote() {
    fn timeout_err() -> CadenceFetchError {
        CadenceFetchError::Timeout
    }
    // Chains succeed; every spot leg times out every cycle. 4 cycles ≥
    // 2× the 2-dirty demotion streak — if Timeout armed the ladder,
    // cycle 3+ would run the split shape.
    let calls = run_lane(timeout_err, None, true, false, 4).await;
    assert_dhan_shape0_every_cycle(&calls, 4, "Timeout");
}

#[tokio::test(start_paused = true)]
async fn test_dhan_sustained_transport_only_cycles_never_demote() {
    fn transport_err() -> CadenceFetchError {
        CadenceFetchError::Transport
    }
    let calls = run_lane(transport_err, None, true, false, 4).await;
    assert_dhan_shape0_every_cycle(&calls, 4, "Transport");
}

#[tokio::test(start_paused = true)]
async fn test_dhan_sustained_empty_only_cycles_never_demote() {
    fn empty_err() -> CadenceFetchError {
        CadenceFetchError::Empty
    }
    let calls = run_lane(empty_err, None, true, false, 4).await;
    assert_dhan_shape0_every_cycle(&calls, 4, "Empty");
}

#[tokio::test(start_paused = true)]
async fn test_dhan_sustained_queue_delay_only_cycles_never_demote() {
    // 2026-07-17 review fix: RUNNER-LEVEL proof that QueueDelay — a
    // SELF-INFLICTED pacing refusal, never a broker signal (F1(iii),
    // 2026-07-15) — never arms the shape ladder. 4 cycles ≥ 2× the
    // 2-dirty demotion streak; QueueDelay is retryable (shared bounded
    // in-cycle retry budget), so the 8-fire shape-0 signature applies.
    fn queue_delay_err() -> CadenceFetchError {
        CadenceFetchError::QueueDelay
    }
    let calls = run_lane(queue_delay_err, None, true, false, 4).await;
    assert_dhan_shape0_every_cycle(&calls, 4, "QueueDelay");
}

/// Groww-lane core assertion: across EVERY observed cycle the 7 NOMINAL
/// wave requests stay CONCURRENT at T+0 (the choice-1 signature) — a
/// demotion to choice 2 would split the wave (chains :01 / spots :02).
fn assert_groww_choice1_every_cycle(calls: &[RecordedCall], cycles: u32, class: &str) {
    for n in 0..cycles {
        let minute = FIRST_CYCLE_MINUTE + 60 * n;
        let inst = all_instants(calls, minute);
        assert!(
            inst.len() >= 7,
            "Groww {class} cycle {}: at least the 7 nominal wave requests (got {})",
            n + 1,
            inst.len()
        );
        assert!(
            inst[6] - inst[0] <= SLOT_TOLERANCE_MS,
            "Groww {class} cycle {}: choice 1 held — all 7 nominal requests \
             concurrent at T+0 ({class} is NON-ARMING); spread {}ms",
            n + 1,
            inst[6] - inst[0]
        );
    }
}

// ---------------------------------------------------------------------------
// (iii) Groww lane: sustained non-429 failures hold choice 1 (all-7 at T+0)
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn test_groww_sustained_timeout_only_cycles_never_demote() {
    fn timeout_err() -> CadenceFetchError {
        CadenceFetchError::Timeout
    }
    // BOTH legs time out every cycle (fully dirty). A demotion to
    // choice 2 would split the wave (chains :01 / spots :02); choice 1
    // fires all 7 nominal requests concurrently at T+0 — pinned for
    // every cycle.
    let calls = run_lane(timeout_err, Some(timeout_err), false, true, 4).await;
    assert_groww_choice1_every_cycle(&calls, 4, "Timeout");
}

#[tokio::test(start_paused = true)]
async fn test_groww_sustained_transport_only_cycles_never_demote() {
    // 2026-07-17 review fix: the Groww proof covered Timeout only —
    // Transport is a distinct non-arming class and gets its own
    // never-demote pin (both legs fail every cycle, fully dirty).
    fn transport_err() -> CadenceFetchError {
        CadenceFetchError::Transport
    }
    let calls = run_lane(transport_err, Some(transport_err), false, true, 4).await;
    assert_groww_choice1_every_cycle(&calls, 4, "Transport");
}

#[tokio::test(start_paused = true)]
async fn test_groww_sustained_empty_only_cycles_never_demote() {
    // 2026-07-17 review fix: Empty (2xx-without-data) is the third
    // non-arming class — sustained fully-dirty Empty cycles must hold
    // choice 1 forever.
    fn empty_err() -> CadenceFetchError {
        CadenceFetchError::Empty
    }
    let calls = run_lane(empty_err, Some(empty_err), false, true, 4).await;
    assert_groww_choice1_every_cycle(&calls, 4, "Empty");
}
