//! Background history re-pull (operator directive 2026-07-20 — "retry
//! native until the 4th second, then cross-fill; **backfill never feeds
//! live decisions**").
//!
//! When a cadence lane's minute resolved via CROSS-FILL, this detached
//! task re-asks the degraded broker's OWN legs at the pinned post-cycle
//! offsets [`CADENCE_HISTORY_REPULL_OFFSETS_MS`] (T+30s / T+50s —
//! deliberately OUTSIDE the next cycle's burst window; T+60s IS the next
//! boundary — `cadence-error-codes.md` §0d H5), exactly 2 attempt waves,
//! so the broker's own rows land in the shared tables via the executor's
//! EXISTING DEDUP-keyed upsert. Record-completeness ONLY.
//!
//! # The isolation law (build-failing ratchet below)
//!
//! This module is HARD-ISOLATED from decisions: it references NO
//! assembly, decision, latch, ladder or cycle-state type — it holds only
//! an executor handle, the gates, and a plain leg plan. A re-pull outcome
//! can therefore NEVER become a resolution value, feed a moneyness fold,
//! arm the shape ladder, or count into a `rate_limited` streak (those
//! all live on the runner's per-cycle state types, which the ratchet
//! test below forbids this module from even naming).
//!
//! # Gate law (§0d — REJECT in review if violated)
//!
//! Every DHAN re-pull request passes `DhanGates::try_acquire_spot` /
//! `try_acquire_chain` immediately before dispatch, NON-BLOCKING: a
//! contended gate SKIPS the leg for that wave (nominal traffic always
//! wins — counted `gate_skipped`, retried at the next wave if any).
//! Groww legs ride the Groww executor's own internal pacing (it has no
//! `DhanGates` by design §4).

use std::sync::Arc;

use tickvault_common::constants::CADENCE_HISTORY_REPULL_OFFSETS_MS;
use tickvault_common::error_code::ErrorCode;
use tickvault_common::feed::Feed;
use tracing::{debug, error};

use super::executor::{
    CadenceExecutor, CadenceFetchError, ChainFetchRequest, SpotFetchRequest, SpotTarget,
};
use super::gate::{DhanGates, GateVerdict};
use super::runner::CadenceClock;
use crate::pipeline::chain_snapshot::ChainUnderlying;

/// One lane's re-pull plan: WHICH legs resolved via cross-fill (the
/// borrower's own rows are missing) + the cycle identity. Plain data —
/// never an assembly/decision handle.
#[derive(Clone, Copy, Debug)]
pub struct HistoryRepullPlan {
    /// The degraded lane whose OWN rows are being re-pulled.
    pub lane: Feed,
    /// The decided minute (IST seconds-of-day minute-open) — every
    /// re-pull request carries THIS minute, so the executor's own
    /// wrong-minute filter (`empty_stale`) remains the stale-answer
    /// guard.
    pub cycle_minute_ist: u32,
    /// The cycle's minute-close boundary, absolute IST ms-of-day (the
    /// re-pull offsets anchor here).
    pub boundary_ms: i64,
    /// Chain legs to re-pull: `Some(expiry_yyyymmdd_opt)` = this
    /// underlying's chain was cross-filled (the expiry was resolved at
    /// spawn time by the RUNNER — this module never touches the
    /// resolver).
    pub chains: [Option<Option<u32>>; ChainUnderlying::COUNT],
    /// Underlying spot legs to re-pull (the VIX spot is never
    /// cross-filled, so it never appears here).
    pub spots: [bool; ChainUnderlying::COUNT],
    /// Per-request timeout for the lane (the runner's own per-lane
    /// value).
    pub request_timeout_ms: i64,
}

impl HistoryRepullPlan {
    /// Any leg still pending?
    #[must_use]
    pub fn any_pending(&self) -> bool {
        self.chains.iter().any(Option::is_some) || self.spots.iter().any(|s| *s)
    }

    /// Count of still-pending legs.
    #[must_use]
    pub fn pending_count(&self) -> u32 {
        let chains = self.chains.iter().filter(|c| c.is_some()).count();
        let spots = self.spots.iter().filter(|s| **s).count();
        // APPROVED: bounded by 6 — the cast is safe.
        #[allow(clippy::cast_possible_truncation)]
        {
            (chains + spots) as u32
        }
    }
}

/// Pure: the remaining sleep to `boundary_ms + offset` from `now_wall`,
/// clamped non-negative (a late spawn — e.g. the cycle wrap-up ran at
/// T+15s — still fires the T+30/T+50 waves at their instants; a
/// pathologically late spawn fires immediately, still bounded to 2
/// waves).
#[must_use]
pub fn repull_sleep_ms(boundary_ms: i64, offset_ms: i64, now_wall_ms: i64) -> i64 {
    boundary_ms
        .saturating_add(offset_ms)
        .saturating_sub(now_wall_ms)
        .max(0)
}

/// One leg's wave outcome (counter label; the pure classification seam).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RepullOutcome {
    /// The broker's own rows arrived (and were upserted by the executor
    /// through the existing DEDUP keys) — the leg is DONE.
    Recovered,
    /// Still 2xx-empty — the leg stays pending for the next wave.
    Empty,
    /// The non-blocking gate acquire was contended — nominal traffic
    /// wins; the leg stays pending for the next wave.
    GateSkipped,
    /// Any error class (incl. a 429 — which, by this module's isolation,
    /// can NEVER arm the shape ladder or feed a `rate_limited` streak).
    Error,
}

impl RepullOutcome {
    /// Static counter label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Recovered => "recovered",
            Self::Empty => "empty",
            Self::GateSkipped => "gate_skipped",
            Self::Error => "error",
        }
    }
}

/// Pure: classify one fetch result into its wave outcome + whether the
/// leg stays pending.
#[must_use]
pub fn classify_repull_result<T>(result: &Result<T, CadenceFetchError>) -> (RepullOutcome, bool) {
    match result {
        Ok(_) => (RepullOutcome::Recovered, false),
        Err(CadenceFetchError::Empty) => (RepullOutcome::Empty, true),
        Err(_) => (RepullOutcome::Error, true),
    }
}

/// Spawn the detached re-pull task for one lane's cross-filled legs.
/// Bounded by construction: 2 waves × ≤6 concurrent bounded fetches;
/// the task holds NO channel into the runner and dies silently at
/// runtime teardown (its longest life is ~T+57s worst case).
// TEST-EXEMPT: thin tokio spawn shell over the unit-tested pure parts (repull_sleep_ms / classify_repull_result / plan bookkeeping); the wiring is pinned by the runner's repull-spawn ratchet test.
pub fn spawn_history_repull<C, E>(
    clock: Arc<C>,
    executor: Arc<E>,
    gates: Option<Arc<DhanGates>>,
    mut plan: HistoryRepullPlan,
) where
    C: CadenceClock,
    E: CadenceExecutor + 'static,
{
    if !plan.any_pending() {
        return;
    }
    drop(tokio::spawn(async move {
        for offset_ms in CADENCE_HISTORY_REPULL_OFFSETS_MS {
            if !plan.any_pending() {
                break;
            }
            let sleep_ms = repull_sleep_ms(plan.boundary_ms, offset_ms, clock.ist_ms_of_day());
            // APPROVED: clamped non-negative — the cast is safe.
            #[allow(clippy::cast_sign_loss)]
            tokio::time::sleep(std::time::Duration::from_millis(sleep_ms as u64)).await;
            run_repull_wave(&clock, &executor, gates.as_deref(), &mut plan).await;
        }
        if plan.any_pending() {
            metrics::counter!(
                "tv_cadence_history_repull_total",
                "lane" => plan.lane.as_str(),
                "outcome" => "exhausted"
            )
            .increment(u64::from(plan.pending_count()));
            // Log-sink-only by design (the Dhan noise-lock posture) —
            // the minute stays cross-filled in the shared tables; later
            // sweeps own any further recovery.
            error!(
                code = ErrorCode::Cadence05RecoveryDegraded.code_str(),
                stage = "repull_exhausted",
                lane = plan.lane.as_str(),
                cycle_minute_ist = plan.cycle_minute_ist,
                pending_legs = plan.pending_count(),
                "CADENCE-05: background history re-pull exhausted its \
                 bounded attempts — the broker still had no own rows for \
                 the cross-filled minute (record recovery deferred to \
                 later sweeps; decisions were NEVER waiting on this)"
            );
        }
    }));
}

/// One wave: gate-check + dispatch every still-pending leg CONCURRENTLY
/// (bounded ≤6 fetches, each under its own request timeout — the wave
/// ends well before the next cycle's burst window).
async fn run_repull_wave<C, E>(
    clock: &Arc<C>,
    executor: &Arc<E>,
    gates: Option<&DhanGates>,
    plan: &mut HistoryRepullPlan,
) where
    C: CadenceClock,
    E: CadenceExecutor + 'static,
{
    let lane = plan.lane;
    let now_mono = clock.monotonic_ms();
    let count = |outcome: RepullOutcome| {
        metrics::counter!(
            "tv_cadence_history_repull_total",
            "lane" => lane.as_str(),
            "outcome" => outcome.as_str()
        )
        .increment(1);
    };

    // Gate pass (Dhan only): non-blocking; a contended leg is skipped
    // for this wave — nominal traffic always wins.
    let mut fire_chains: Vec<(usize, Option<u32>)> = Vec::new();
    let mut fire_spots: Vec<usize> = Vec::new();
    for (i, entry) in plan.chains.iter().enumerate() {
        let Some(expiry) = entry else { continue };
        let admitted = match gates {
            Some(g) => {
                g.try_acquire_chain(ChainUnderlying::ALL[i], *expiry, now_mono)
                    == GateVerdict::Acquired
            }
            None => true,
        };
        if admitted {
            fire_chains.push((i, *expiry));
        } else {
            count(RepullOutcome::GateSkipped);
            debug!(
                lane = lane.as_str(),
                underlying_idx = i,
                "cadence history re-pull: chain leg gate-contended — skipped this wave"
            );
        }
    }
    for (i, pending) in plan.spots.iter().enumerate() {
        if !*pending {
            continue;
        }
        let admitted = match gates {
            Some(g) => g.try_acquire_spot(now_mono) == GateVerdict::Acquired,
            None => true,
        };
        if admitted {
            fire_spots.push(i);
        } else {
            count(RepullOutcome::GateSkipped);
            debug!(
                lane = lane.as_str(),
                underlying_idx = i,
                "cadence history re-pull: spot leg gate-contended — skipped this wave"
            );
        }
    }

    // Concurrent bounded dispatch (the executor upserts through the
    // existing DEDUP keys internally; this task never sees row data).
    // APPROVED: validated > 0 at boot — the cast is safe.
    #[allow(clippy::cast_sign_loss)]
    let timeout = std::time::Duration::from_millis(plan.request_timeout_ms.max(1) as u64);
    let chain_futs = fire_chains.iter().map(|(i, expiry)| {
        let req = ChainFetchRequest {
            feed: lane,
            underlying: ChainUnderlying::ALL[*i],
            expiry_yyyymmdd: *expiry,
            cycle_minute_ist: plan.cycle_minute_ist,
            deadline_epoch_ms: clock.epoch_ms().saturating_add(plan.request_timeout_ms),
        };
        let exec = Arc::clone(executor);
        async move {
            match tokio::time::timeout(timeout, exec.fetch_chain(req)).await {
                Ok(r) => r.map(|_| ()),
                Err(_elapsed) => Err(CadenceFetchError::Timeout),
            }
        }
    });
    let spot_targets: Vec<SpotTarget> = fire_spots
        .iter()
        .map(|i| match ChainUnderlying::ALL[*i] {
            ChainUnderlying::Nifty => SpotTarget::Nifty,
            ChainUnderlying::Banknifty => SpotTarget::BankNifty,
            ChainUnderlying::Sensex => SpotTarget::Sensex,
        })
        .collect();
    let spot_futs = spot_targets.iter().map(|target| {
        let req = SpotFetchRequest {
            feed: lane,
            target: *target,
            cycle_minute_ist: plan.cycle_minute_ist,
            deadline_epoch_ms: clock.epoch_ms().saturating_add(plan.request_timeout_ms),
        };
        let exec = Arc::clone(executor);
        async move {
            match tokio::time::timeout(timeout, exec.fetch_spot(req)).await {
                Ok(r) => r.map(|_| ()),
                Err(_elapsed) => Err(CadenceFetchError::Timeout),
            }
        }
    });
    let (chain_results, spot_results) = futures_util::future::join(
        futures_util::future::join_all(chain_futs),
        futures_util::future::join_all(spot_futs),
    )
    .await;

    for ((i, _), result) in fire_chains.iter().zip(chain_results.iter()) {
        let (outcome, still_pending) = classify_repull_result(result);
        count(outcome);
        if !still_pending {
            plan.chains[*i] = None;
        }
    }
    for (i, result) in fire_spots.iter().zip(spot_results.iter()) {
        let (outcome, still_pending) = classify_repull_result(result);
        count(outcome);
        if !still_pending {
            plan.spots[*i] = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_repull_sleep_ms_clamps_and_targets() {
        // On-time spawn: wrap-up at T+15s targets T+30s → 15s sleep.
        assert_eq!(repull_sleep_ms(1_000_000, 30_000, 1_015_000), 15_000);
        // Second wave from T+30.5s targets T+50s.
        assert_eq!(repull_sleep_ms(1_000_000, 50_000, 1_030_500), 19_500);
        // Pathologically late spawn: fires immediately, never negative.
        assert_eq!(repull_sleep_ms(1_000_000, 30_000, 1_040_000), 0);
    }

    #[test]
    fn test_classify_repull_result_covers_all_classes() {
        let ok: Result<(), CadenceFetchError> = Ok(());
        assert_eq!(
            classify_repull_result(&ok),
            (RepullOutcome::Recovered, false)
        );
        let empty: Result<(), CadenceFetchError> = Err(CadenceFetchError::Empty);
        assert_eq!(classify_repull_result(&empty), (RepullOutcome::Empty, true));
        // A re-pull 429 is a plain Error here — by the isolation law it
        // can never arm the shape ladder or feed a rate_limited streak
        // (this module cannot even name those types).
        let rl: Result<(), CadenceFetchError> = Err(CadenceFetchError::RateLimited {
            retry_after_ms: None,
        });
        assert_eq!(classify_repull_result(&rl), (RepullOutcome::Error, true));
        let mal: Result<(), CadenceFetchError> = Err(CadenceFetchError::Malformed);
        assert_eq!(classify_repull_result(&mal), (RepullOutcome::Error, true));
    }

    #[test]
    fn test_plan_pending_bookkeeping() {
        let mut plan = HistoryRepullPlan {
            lane: Feed::Dhan,
            cycle_minute_ist: 33_240,
            boundary_ms: 33_300_000,
            chains: [Some(Some(20_260_723)), None, None],
            spots: [true, false, false],
            request_timeout_ms: 5_000,
        };
        assert!(plan.any_pending());
        assert_eq!(plan.pending_count(), 2);
        plan.chains[0] = None;
        plan.spots[0] = false;
        assert!(!plan.any_pending());
        assert_eq!(plan.pending_count(), 0);
    }

    /// The operator isolation law, build-failing (plan test item): the
    /// re-pull module must reference NO assembly / decision / ladder /
    /// cycle-state machinery — a re-pull can never feed a live decision,
    /// arm a ladder, or count into a rate_limited streak.
    #[test]
    fn test_history_repull_isolation_ratchet() {
        let src = include_str!("history_repull.rs");
        let production: &str = src
            .split("#[cfg(test)]")
            .next()
            .expect("split always yields a head");
        for banned in [
            "assembly::",
            "LaneAssembly",
            "decision::",
            "DecisionLatch",
            "DecisionSnapshot",
            "emit_decision",
            "ladder::",
            "StreakLadder",
            "failure_arms_ladder",
            "CycleState",
            "LaneRun",
            "DegradeFlags",
            "record_spot",
            "record_chain",
            "cross_fill",
        ] {
            assert!(
                !production.contains(banned),
                "isolation breach: history_repull production code references `{banned}`"
            );
        }
    }
}
