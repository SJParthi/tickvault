//! ITEM 5 (#1693): background history re-pull executor for cross-filled
//! cadence lanes.
//!
//! When a cycle lane resolves via `cross_fill`, the sibling feed supplied the
//! minute's rows but the lane's OWN vendor history for that minute is still
//! missing. This module runs the bounded repair: two background attempts at
//! the `CADENCE_HISTORY_REPULL_OFFSETS_MS` marks (T+30s / T+50s from the
//! decided minute's close) re-fetch the lane's own chain + spot rows through
//! the SAME `CadenceExecutor` the scheduler uses. The executor persists
//! internally, so this task holds no persistence handle and never re-enters
//! cycle state (pinned by `cadence_history_repull_isolation_guard`).
//!
//! Contract (plan + 2026-07-20 amendment):
//! - fire-and-forget: spawned from the runner's E6 seam; never blocks the
//!   scheduler.
//! - Dhan legs pass the SHARED `DhanGates` rings on the runner's own
//!   monotonic timebase (anchored at spawn); a gate refusal SKIPS the leg
//!   for that attempt - the repair task never sleeps on `RetryAtMs`.
//! - Groww is gate-free (`gates: None`).
//! - chains fire before spots; an unresolved expiry skips that chain leg
//!   (the repair path never guesses an expiry).
//! - a leg recovered on an earlier attempt is not re-fetched.
//! - any `RateLimited` reply aborts the WHOLE remaining re-pull - this path
//!   must never add 429 pressure.
//! - each attempt's leg sweep is hard-bounded by
//!   `CADENCE_HISTORY_REPULL_TIMEOUT_MS`.
//! - outcomes: `tv_cadence_history_repull_total{lane, outcome}` with
//!   outcome in recovered | empty | error | aborted_429 | gate_skipped.

use std::sync::Arc;

use tokio::time::{Duration, Instant, sleep_until, timeout};
use tracing::info;

use tickvault_common::constants::{
    CADENCE_HISTORY_REPULL_OFFSETS_MS, CADENCE_HISTORY_REPULL_TIMEOUT_MS,
};
use tickvault_common::feed::Feed;

use crate::pipeline::chain_snapshot::ChainUnderlying;

use super::executor::{
    CadenceExecutor, CadenceFetchError, ChainFetchRequest, SpotFetchRequest, SpotTarget,
};
use super::gate::{DhanGates, GateVerdict};

/// Timebase anchor captured at spawn so gate calls share the runner clock's
/// monotonic epoch and request deadlines share its wall epoch - both advance
/// by the same tokio-`Instant` elapsed, so no clock handle crosses into the
/// detached task.
pub struct RepullAnchor {
    /// `clock.monotonic_ms()` at spawn (the shared gate-ring timebase).
    pub spawn_mono_ms: i64,
    /// `clock.epoch_ms()` at spawn (request-deadline wall base).
    pub spawn_epoch_ms: i64,
    /// Instant captured at spawn; elapsed since it advances both bases.
    pub spawn_instant: Instant,
}

impl RepullAnchor {
    fn elapsed_ms(&self) -> i64 {
        // APPROVED: elapsed ms since spawn fits i64 for ~292M years.
        #[allow(clippy::cast_possible_wrap, clippy::cast_possible_truncation)]
        {
            self.spawn_instant.elapsed().as_millis() as i64
        }
    }

    fn now_mono_ms(&self) -> i64 {
        self.spawn_mono_ms.saturating_add(self.elapsed_ms())
    }

    fn now_epoch_ms(&self) -> i64 {
        self.spawn_epoch_ms.saturating_add(self.elapsed_ms())
    }
}

/// Everything the detached repair task owns. Built by the runner's spawn
/// seam; fields are plain data + Arcs so the task is `'static`.
pub struct HistoryRepullCtx<E> {
    pub feed: Feed,
    pub cycle_minute_ist: u32,
    pub executor: Arc<E>,
    /// Dhan lanes carry the SHARED gate rings; Groww passes `None`.
    pub gates: Option<Arc<DhanGates>>,
    /// (underlying, day-locked expiry) resolved AT SPAWN by the runner -
    /// a `None` expiry means that chain leg is skipped (never guessed).
    pub chain_expiries: Vec<(ChainUnderlying, Option<u32>)>,
    pub anchor: RepullAnchor,
    /// ms already elapsed past the decided minute's close when the spawn
    /// happened - subtracted from the nominal offsets so attempts land at
    /// the absolute T+30s / T+50s marks.
    pub elapsed_in_cycle_ms: i64,
}

/// Attempt delays relative to "now", derived from the nominal close-relative
/// offsets; lag never pushes an attempt earlier than immediately.
pub(crate) fn repull_delays_ms(elapsed_in_cycle_ms: i64) -> [i64; 2] {
    [
        CADENCE_HISTORY_REPULL_OFFSETS_MS[0]
            .saturating_sub(elapsed_in_cycle_ms)
            .max(0),
        CADENCE_HISTORY_REPULL_OFFSETS_MS[1]
            .saturating_sub(elapsed_in_cycle_ms)
            .max(0),
    ]
}

pub(crate) fn is_rate_limited(err: &CadenceFetchError) -> bool {
    matches!(err, CadenceFetchError::RateLimited { .. })
}

fn count_outcome(lane: &'static str, outcome: &'static str) {
    metrics::counter!(
        "tv_cadence_history_repull_total",
        "lane" => lane,
        "outcome" => outcome
    )
    .increment(1);
}

enum AttemptVerdict {
    Continue,
    Abort429,
}

/// Run the bounded two-attempt repair. Consumes the ctx; the runner spawns
/// this future on a detached task.
pub async fn run_history_repull<E: CadenceExecutor>(ctx: HistoryRepullCtx<E>) {
    let lane = ctx.feed.as_str();
    let delays = repull_delays_ms(ctx.elapsed_in_cycle_ms);
    let mut chain_done = vec![false; ctx.chain_expiries.len()];
    let mut spot_done = vec![false; SpotTarget::ALL.len()];
    for (attempt_idx, delay_ms) in delays.iter().enumerate() {
        let due =
            ctx.anchor.spawn_instant + Duration::from_millis(u64::try_from(*delay_ms).unwrap_or(0));
        sleep_until(due).await;
        if chain_done.iter().all(|d| *d) && spot_done.iter().all(|d| *d) {
            break; // everything recovered on an earlier attempt
        }
        let bound = Duration::from_millis(CADENCE_HISTORY_REPULL_TIMEOUT_MS);
        match timeout(bound, run_attempt(&ctx, &mut chain_done, &mut spot_done)).await {
            Ok(AttemptVerdict::Continue) => {}
            Ok(AttemptVerdict::Abort429) => {
                count_outcome(lane, "aborted_429");
                info!(
                    lane,
                    cycle_minute_ist = ctx.cycle_minute_ist,
                    attempt = attempt_idx,
                    "cadence: history re-pull aborted on 429 (remaining \
                     attempts cancelled)"
                );
                return;
            }
            Err(_elapsed) => {
                info!(
                    lane,
                    cycle_minute_ist = ctx.cycle_minute_ist,
                    attempt = attempt_idx,
                    "cadence: history re-pull attempt hit its hard bound"
                );
            }
        }
    }
}

async fn run_attempt<E: CadenceExecutor>(
    ctx: &HistoryRepullCtx<E>,
    chain_done: &mut [bool],
    spot_done: &mut [bool],
) -> AttemptVerdict {
    let lane = ctx.feed.as_str();
    // Chains first (mirror of the cycle volley order).
    for (i, (underlying, expiry)) in ctx.chain_expiries.iter().enumerate() {
        if chain_done[i] {
            continue;
        }
        let Some(expiry_yyyymmdd) = *expiry else {
            info!(
                lane,
                underlying = ?underlying,
                "cadence: history re-pull skipping chain leg (expiry \
                 unresolved - the repair path never guesses)"
            );
            continue;
        };
        if let Some(gates) = ctx.gates.as_deref() {
            match gates.try_acquire_chain(
                *underlying,
                Some(expiry_yyyymmdd),
                ctx.anchor.now_mono_ms(),
            ) {
                GateVerdict::Acquired => {}
                GateVerdict::RetryAtMs(_) => {
                    count_outcome(lane, "gate_skipped");
                    continue; // never waits on the shared rings
                }
            }
        }
        let req = ChainFetchRequest {
            feed: ctx.feed,
            underlying: *underlying,
            expiry_yyyymmdd: Some(expiry_yyyymmdd),
            cycle_minute_ist: ctx.cycle_minute_ist,
            deadline_epoch_ms: ctx.anchor.now_epoch_ms().saturating_add(
                i64::try_from(CADENCE_HISTORY_REPULL_TIMEOUT_MS).unwrap_or(i64::MAX),
            ),
        };
        match ctx.executor.fetch_chain(req).await {
            Ok(_ok) => {
                chain_done[i] = true;
                count_outcome(lane, "recovered");
            }
            Err(e) if is_rate_limited(&e) => return AttemptVerdict::Abort429,
            Err(CadenceFetchError::Empty) => count_outcome(lane, "empty"),
            Err(_e) => count_outcome(lane, "error"),
        }
    }
    // Spots second.
    for (t_idx, target) in SpotTarget::ALL.iter().enumerate() {
        if spot_done[t_idx] {
            continue;
        }
        if let Some(gates) = ctx.gates.as_deref() {
            match gates.try_acquire_spot(ctx.anchor.now_mono_ms()) {
                GateVerdict::Acquired => {}
                GateVerdict::RetryAtMs(_) => {
                    count_outcome(lane, "gate_skipped");
                    continue; // never waits on the shared rings
                }
            }
        }
        let req = SpotFetchRequest {
            feed: ctx.feed,
            target: *target,
            cycle_minute_ist: ctx.cycle_minute_ist,
            deadline_epoch_ms: ctx.anchor.now_epoch_ms().saturating_add(
                i64::try_from(CADENCE_HISTORY_REPULL_TIMEOUT_MS).unwrap_or(i64::MAX),
            ),
        };
        match ctx.executor.fetch_spot(req).await {
            Ok(_snap) => {
                spot_done[t_idx] = true;
                count_outcome(lane, "recovered");
            }
            Err(e) if is_rate_limited(&e) => return AttemptVerdict::Abort429,
            Err(CadenceFetchError::Empty) => count_outcome(lane, "empty"),
            Err(_e) => count_outcome(lane, "error"),
        }
    }
    AttemptVerdict::Continue
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use super::super::executor::{ChainFetchOk, ExpiryListRequest, SpotSnapshot};
    use super::*;

    fn ok_chain() -> ChainFetchOk {
        ChainFetchOk {
            underlying_spot: None,
            published_to_registry: false,
        }
    }

    fn ok_spot() -> SpotSnapshot {
        SpotSnapshot {
            price: 1.0,
            source_minute_ist: 0,
            received_at_epoch_ms: 0,
        }
    }

    #[derive(Default)]
    struct ScriptedExecutor {
        chain_calls: Mutex<Vec<(usize, Option<u32>)>>,
        spot_calls: Mutex<Vec<u32>>,
        /// Popped once per fetch (chains then spots, in call order);
        /// exhausted queue means Ok.
        replies: Mutex<VecDeque<Result<(), CadenceFetchError>>>,
    }

    impl CadenceExecutor for ScriptedExecutor {
        async fn fetch_chain(
            &self,
            req: ChainFetchRequest,
        ) -> Result<ChainFetchOk, CadenceFetchError> {
            self.chain_calls
                .lock()
                .unwrap()
                .push((req.underlying as usize, req.expiry_yyyymmdd));
            let reply = self.replies.lock().unwrap().pop_front();
            match reply {
                Some(Err(e)) => Err(e),
                _ => Ok(ok_chain()),
            }
        }

        async fn fetch_spot(
            &self,
            req: SpotFetchRequest,
        ) -> Result<SpotSnapshot, CadenceFetchError> {
            self.spot_calls.lock().unwrap().push(req.cycle_minute_ist);
            let reply = self.replies.lock().unwrap().pop_front();
            match reply {
                Some(Err(e)) => Err(e),
                _ => Ok(ok_spot()),
            }
        }

        async fn fetch_expiry_list(
            &self,
            _req: ExpiryListRequest,
        ) -> Result<Vec<u32>, CadenceFetchError> {
            Ok(Vec::new())
        }
    }

    fn anchor() -> RepullAnchor {
        RepullAnchor {
            spawn_mono_ms: 0,
            spawn_epoch_ms: 0,
            spawn_instant: Instant::now(),
        }
    }

    fn all_expiries() -> Vec<(ChainUnderlying, Option<u32>)> {
        ChainUnderlying::ALL
            .iter()
            .map(|&u| (u, Some(20_260_723)))
            .collect()
    }

    #[test]
    fn test_repull_delays_ms_schedule_pinned() {
        assert_eq!(repull_delays_ms(0), [30_000, 50_000]);
        assert_eq!(repull_delays_ms(6_000), [24_000, 44_000]);
        assert_eq!(repull_delays_ms(60_000), [0, 0]);
    }

    #[tokio::test]
    async fn test_run_history_repull_dedup_recovered_leg_not_refetched() {
        let exec = Arc::new(ScriptedExecutor::default());
        let ctx = HistoryRepullCtx {
            feed: Feed::Groww,
            cycle_minute_ist: 34_200,
            executor: Arc::clone(&exec),
            gates: None,
            chain_expiries: all_expiries(),
            anchor: anchor(),
            elapsed_in_cycle_ms: 60_000, // both attempts due immediately
        };
        run_history_repull(ctx).await;
        assert_eq!(exec.chain_calls.lock().unwrap().len(), 3);
        assert_eq!(exec.spot_calls.lock().unwrap().len(), SpotTarget::ALL.len());
    }

    #[tokio::test]
    async fn test_run_history_repull_429_aborts_remaining() {
        let exec = Arc::new(ScriptedExecutor::default());
        exec.replies
            .lock()
            .unwrap()
            .push_back(Err(CadenceFetchError::RateLimited {
                retry_after_ms: None,
            }));
        let ctx = HistoryRepullCtx {
            feed: Feed::Groww,
            cycle_minute_ist: 34_200,
            executor: Arc::clone(&exec),
            gates: None,
            chain_expiries: all_expiries(),
            anchor: anchor(),
            elapsed_in_cycle_ms: 60_000,
        };
        run_history_repull(ctx).await;
        assert_eq!(exec.chain_calls.lock().unwrap().len(), 1);
        assert!(exec.spot_calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_run_history_repull_gate_refusal_skips_never_waits() {
        let exec = Arc::new(ScriptedExecutor::default());
        // Saturate every ring so each acquire refuses (huge chain spacing +
        // pre-consumed slots).
        let gates = Arc::new(DhanGates::new(600_000, 1));
        for u in ChainUnderlying::ALL {
            let _ = gates.try_acquire_chain(*u, Some(20_260_723), 0);
        }
        let _ = gates.try_acquire_spot(0);
        let started = std::time::Instant::now();
        let ctx = HistoryRepullCtx {
            feed: Feed::Dhan,
            cycle_minute_ist: 34_200,
            executor: Arc::clone(&exec),
            gates: Some(gates),
            chain_expiries: all_expiries(),
            anchor: anchor(),
            elapsed_in_cycle_ms: 60_000,
        };
        run_history_repull(ctx).await;
        assert!(exec.chain_calls.lock().unwrap().is_empty());
        assert!(exec.spot_calls.lock().unwrap().is_empty());
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "gate refusal must skip, never wait out RetryAtMs"
        );
    }

    #[tokio::test]
    async fn test_run_history_repull_expiry_unresolved_skips_leg() {
        let exec = Arc::new(ScriptedExecutor::default());
        let ctx = HistoryRepullCtx {
            feed: Feed::Groww,
            cycle_minute_ist: 34_200,
            executor: Arc::clone(&exec),
            gates: None,
            chain_expiries: vec![
                (ChainUnderlying::Nifty, None),
                (ChainUnderlying::Banknifty, Some(20_260_723)),
            ],
            anchor: anchor(),
            elapsed_in_cycle_ms: 60_000,
        };
        run_history_repull(ctx).await;
        assert_eq!(
            *exec.chain_calls.lock().unwrap(),
            vec![(ChainUnderlying::Banknifty as usize, Some(20_260_723))]
        );
        assert_eq!(exec.spot_calls.lock().unwrap().len(), SpotTarget::ALL.len());
    }
}
