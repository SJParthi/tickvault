//! THE PROOF — deterministic zero-429 replay of the judge-locked cadence
//! (design §11): a fully deterministic simulation (SimClock, scripted
//! outcomes, NO tokio timers) drives the REAL pure components — the
//! schedule (`next_joinable_boundary` / `build_cycle_slots`), the CAS
//! gates (`DhanGates`), the failure ladder and the decision latch —
//! through 64-cycle days under boot-skew / wake-jitter / GC-pause /
//! latency / failure-outcome / restart permutations and asserts the
//! STRUCTURAL rate floors:
//!
//! - per-underlying chain fire deltas ≥ 3000 ms (broker wall domain)
//! - GLOBAL chain fire deltas ≥ 3000 ms — across underlyings, retries,
//!   ladder rungs AND process restarts
//! - NEVER more than `spot_window_cap` Dhan spot authorizations in ANY
//!   rolling 1000ms window — across every concurrency-ladder step,
//!   step transition, anchor rung, retry and restart (the 2026-07-15
//!   rolling-window gate change; Dhan hard cap 5/sec)
//! - zero gate denials on nominal slots (on-time dispatch)
//! - exactly 1 decision per (lane, cycle) — the latch admits every fresh
//!   pair and refuses every repeat
//! - a DECIDED outcome is never emitted past the lane cutoff (the sim
//!   mirrors the runner's `finalize_if_complete` now≤cutoff guard: a
//!   permutation whose data completes only past the cutoff must
//!   honest-skip — deleting the guard from the mirror fails the assert
//!   on late-completing permutations); every skip carries a typed reason
//! - every successful chain fetch "publishes" its snapshot EXACTLY once
//!   per (underlying, cycle) — incl. LATE successes (audit-only for the
//!   decision, never dropped from the registry, never refetched). The
//!   executor-side publish itself is a contract of the LATER real-broker
//!   executor PR — this pins the retry policy's never-refetch-a-success
//!   half, the only half that exists in this PR.
//! - the replay is NEVER vacuous: all 64 cycles must complete and the
//!   ledger must carry every cycle's 3 chain primaries + 4 spot singles
//!   (a boundary-selection regression cannot silently green the proof)
//!
//! Plus the deterministic named races (minute-boundary double-fire,
//! restart-mid-cycle, retry-through-gate) and the #1540-consumption
//! boundary tests (ATM grid, all-Unknown surfacing, empty-chain
//! sentinel, 200-empty spot → chain-embedded fallback end-to-end).

use proptest::prelude::*;
use tickvault_common::config::CadenceConfig;
use tickvault_common::constants::CADENCE_SPOT_WINDOW_MS;
use tickvault_common::feed::Feed;
use tickvault_common::moneyness::{Moneyness, OptionLeg};
use tickvault_core::cadence::assembly::{
    ChainCell, ChainProvenance, LaneAssembly, SpotProvenance, fold_chain_cell_moneyness,
    fold_chain_moneyness,
};
use tickvault_core::cadence::decision::{DecisionLatch, DecisionOutcome, SkipReason};
use tickvault_core::cadence::executor::{CadenceFetchError, SpotTarget};
use tickvault_core::cadence::gate::{DhanGates, GateVerdict};
use tickvault_core::cadence::ladder::{
    CycleVerdict, GROWW_SHAPE_MAX_STEP, SPOT_CONCURRENCY_MAX_STEP, StreakLadder,
    failure_arms_ladder, may_retry_in_cycle, min_spot_step_for_cap, next_rung,
};
use tickvault_core::cadence::schedule::{
    CADENCE_RETRY_LATENCY_ALLOWANCE_MS, CycleSlots, build_cycle_slots, next_joinable_boundary,
};
use tickvault_core::pipeline::chain_snapshot::{
    ChainMoneynessSnapshot, ChainUnderlying, SnapshotRow, load_chain_snapshot,
    publish_chain_snapshot,
};

// ---------------------------------------------------------------------------
// Sim scaffolding
// ---------------------------------------------------------------------------

/// Deterministic monotonic+wall clock pair. Both advance in lockstep
/// (real time); a RESTART re-anchors the monotonic origin (a fresh
/// process's monotonic domain knows nothing of the old one's).
#[derive(Clone, Copy, Debug)]
struct SimClock {
    /// IST wall milliseconds-of-day (absolute, shared with the broker).
    wall_ms: i64,
    /// Monotonic origin: `mono = wall - origin` (re-anchored on restart).
    mono_origin: i64,
}

impl SimClock {
    fn mono(&self) -> i64 {
        self.wall_ms - self.mono_origin
    }

    fn advance_to_wall(&mut self, wall_ms: i64) {
        self.wall_ms = self.wall_ms.max(wall_ms);
    }

    /// A process restart: wall continues, monotonic re-anchors.
    fn restart(&mut self, new_origin_skew: i64) {
        self.mono_origin = self.wall_ms + new_origin_skew;
    }
}

/// Tiny deterministic xorshift so one u64 proptest seed scripts every
/// per-event value (jitter / GC / latency / outcome) without giant
/// strategy inputs.
struct SimRng(u64);

impl SimRng {
    fn next(&mut self) -> u64 {
        let mut x = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        self.0 = x;
        x ^= x >> 30;
        x = x.wrapping_mul(0xBF58_476D_1CE4_E5B9);
        x ^= x >> 27;
        x = x.wrapping_mul(0x94D0_49BB_1331_11EB);
        x ^ (x >> 31)
    }

    /// Uniform in `[0, bound)` (bound > 0).
    fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound
    }
}

/// Scripted fetch outcome distribution (design §11 strategy row).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SimOutcome {
    Ok,
    Timeout,
    Transport,
    RateLimited,
    Http5xx,
    Empty200,
}

impl SimOutcome {
    fn draw(rng: &mut SimRng, fail_bias_pct: u64) -> Self {
        if rng.below(100) >= fail_bias_pct {
            return Self::Ok;
        }
        match rng.below(5) {
            0 => Self::Timeout,
            1 => Self::Transport,
            2 => Self::RateLimited,
            3 => Self::Http5xx,
            _ => Self::Empty200,
        }
    }

    /// The executor-seam mapping (5xx folds into Transport).
    fn as_error(self) -> Option<CadenceFetchError> {
        match self {
            Self::Ok => None,
            Self::Timeout => Some(CadenceFetchError::Timeout),
            Self::Transport | Self::Http5xx => Some(CadenceFetchError::Transport),
            Self::RateLimited => Some(CadenceFetchError::RateLimited {
                retry_after_ms: None,
            }),
            Self::Empty200 => Some(CadenceFetchError::Empty),
        }
    }
}

/// Broker-side fire ledger: WALL instants of every ACQUIRED fire, per
/// gate key — the domain a real broker rate-limits in, surviving
/// restarts (the monotonic domain does not).
#[derive(Default)]
struct FireLedger {
    per_underlying: [Vec<i64>; ChainUnderlying::COUNT],
    chain_global: Vec<i64>,
    spot: Vec<i64>,
}

impl FireLedger {
    fn assert_floors(&self, chain_spacing: i64, spot_window_cap: u32, case: &str) {
        for (i, fires) in self.per_underlying.iter().enumerate() {
            assert_sorted_deltas(fires, chain_spacing, &format!("{case}: per-UL chain #{i}"));
        }
        assert_sorted_deltas(
            &self.chain_global,
            chain_spacing,
            &format!("{case}: GLOBAL chain"),
        );
        assert_window_cap(
            &self.spot,
            CADENCE_SPOT_WINDOW_MS,
            spot_window_cap,
            &format!("{case}: spot"),
        );
    }
}

fn assert_sorted_deltas(fires: &[i64], min_delta: i64, what: &str) {
    for w in fires.windows(2) {
        assert!(
            w[1] - w[0] >= min_delta,
            "{what}: fire delta {} < {min_delta} (at wall {} → {})",
            w[1] - w[0],
            w[0],
            w[1]
        );
    }
}

/// The rolling-window invariant on a SORTED fire ledger: no sliding
/// `window_ms` window may hold more than `cap` fires ⟺ every `cap + 1`
/// consecutive fires span ≥ `window_ms` (2026-07-15 gate change).
fn assert_window_cap(fires: &[i64], window_ms: i64, cap: u32, what: &str) {
    let cap = cap as usize;
    for w in fires.windows(cap + 1) {
        assert!(
            w[cap] - w[0] >= window_ms,
            "{what}: {} fires inside one rolling {window_ms}ms window \
             (wall {} → {} spans {})",
            cap + 1,
            w[0],
            w[cap],
            w[cap] - w[0]
        );
    }
}

/// One lane's per-cycle sim verdict.
#[derive(Debug)]
struct SimLaneOutcome {
    outcome: DecisionOutcome,
    emitted_at_wall: i64,
}

/// Attempt one gated Dhan fire at `target_wall` (+`jitter`): advances the
/// clock, defers through the gate to the authorized instant, records the
/// acquired fire in the ledger. Returns the acquired wall instant.
/// `nominal_denials` counts a deferral of a NOMINAL fire dispatched
/// on-time (the should-never gate-bug signal — the runner's
/// dispatch-lateness demotion mirrored here).
#[allow(clippy::too_many_arguments)]
fn sim_gated_chain_fire(
    clock: &mut SimClock,
    gates: &DhanGates,
    ledger: &mut FireLedger,
    underlying: ChainUnderlying,
    target_wall: i64,
    jitter: i64,
    nominal: bool,
    cycle_dispatched_late: &mut bool,
    nominal_denials: &mut u32,
) -> i64 {
    clock.advance_to_wall(target_wall + jitter);
    if clock.wall_ms - target_wall > 0 {
        *cycle_dispatched_late = true;
    }
    loop {
        match gates.try_acquire_chain(underlying, clock.mono()) {
            GateVerdict::Acquired => {
                ledger.per_underlying[underlying.index()].push(clock.wall_ms);
                ledger.chain_global.push(clock.wall_ms);
                return clock.wall_ms;
            }
            GateVerdict::RetryAtMs(at_mono) => {
                if nominal && !*cycle_dispatched_late {
                    *nominal_denials += 1;
                }
                let wall_at = clock.wall_ms + (at_mono - clock.mono());
                clock.advance_to_wall(wall_at);
            }
        }
    }
}

/// Attempt one gated Dhan spot fire (mirror of the chain path).
#[allow(clippy::too_many_arguments)]
fn sim_gated_spot_fire(
    clock: &mut SimClock,
    gates: &DhanGates,
    ledger: &mut FireLedger,
    target_wall: i64,
    jitter: i64,
    nominal: bool,
    cycle_dispatched_late: &mut bool,
    nominal_denials: &mut u32,
) -> i64 {
    clock.advance_to_wall(target_wall + jitter);
    if clock.wall_ms - target_wall > 0 {
        *cycle_dispatched_late = true;
    }
    loop {
        match gates.try_acquire_spot(clock.mono()) {
            GateVerdict::Acquired => {
                ledger.spot.push(clock.wall_ms);
                return clock.wall_ms;
            }
            GateVerdict::RetryAtMs(at_mono) => {
                if nominal && !*cycle_dispatched_late {
                    *nominal_denials += 1;
                }
                let wall_at = clock.wall_ms + (at_mono - clock.mono());
                clock.advance_to_wall(wall_at);
            }
        }
    }
}

/// One deferred Dhan in-cycle retry (mirrors the runner's `insert_event`
/// ordering: a retry enters the SAME time-sorted event queue as the
/// remaining primaries — it fires at its own grid slot AFTER them, never
/// preempting a later primary. The pre-fix inline refire time-warped the
/// clock past the remaining primaries' targets, making them dispatch
/// "late" and self-demoting the nominal-denial gate-bug signal for the
/// rest of the cycle).
enum SimRetry {
    Chain { underlying_idx: usize },
    Spot { target_idx: usize },
}

/// Simulate ONE Dhan-lane cycle at `slots`: 3 gated chain primaries +
/// 4 gated spot fires grouped by the cycle's concurrency-ladder step,
/// then the merged (time-sorted) in-cycle retry grid — the production
/// event-queue fire SEQUENCE. Every successful chain fetch records one
/// registry "publish" into `publishes` (incl. LATE successes —
/// audit-only for the decision, never dropped, never refetched).
/// Returns (arming_failure_seen, spot_dirty_seen, lane outcome) —
/// `spot_dirty` = ≥1 SPOT outcome RateLimited (the concurrency ladder's
/// dirty signal, 2026-07-15).
#[allow(clippy::too_many_lines)]
#[allow(clippy::too_many_arguments)]
fn sim_dhan_cycle(
    clock: &mut SimClock,
    gates: &DhanGates,
    ledger: &mut FireLedger,
    slots: &CycleSlots,
    cfg: &CadenceConfig,
    rng: &mut SimRng,
    fail_bias_pct: u64,
    nominal_denials: &mut u32,
    post_reseed: bool,
    publishes: &mut Vec<(usize, u32)>,
) -> (bool, bool, SimLaneOutcome) {
    // The first cycle after a boot/restart reseed demotes nominal
    // deferrals (the reseed's deliberate one-slot hold — the runner's
    // `demote_nominal` mirror).
    let mut dispatched_late = post_reseed;
    let mut arming = false;
    let mut spot_dirty = false;
    let mut chains_ok = [false; ChainUnderlying::COUNT];
    let mut spots_ok = [false; 4];
    let mut retries_used_chain = [0_u32; ChainUnderlying::COUNT];
    let mut retries_used_spot = [0_u32; 4];
    let mut next_retry_slot = 0_usize;
    let mut retry_queue: Vec<(i64, SimRetry)> = Vec::new();
    // The instant the LAST required (non-VIX) leg completed usably —
    // the event-driven decide instant.
    let mut decide_ready_at = i64::MIN;

    // Chain primaries (nominal), in slot order — a failure only QUEUES
    // its retry at its grid slot (the runner inserts the retry into the
    // sorted event queue; primaries keep their own slots).
    for (i, u) in ChainUnderlying::ALL.iter().enumerate() {
        let jitter = i64::try_from(rng.below(500)).unwrap_or(0);
        let fired_at = sim_gated_chain_fire(
            clock,
            gates,
            ledger,
            *u,
            slots.dhan_chain_slots_ms[i],
            jitter,
            true,
            &mut dispatched_late,
            nominal_denials,
        );
        let latency = 10 + i64::try_from(rng.below(7_990)).unwrap_or(0);
        let outcome = SimOutcome::draw(rng, fail_bias_pct);
        let completed_at = fired_at + latency;
        match outcome.as_error() {
            None => {
                // The executor publishes EVERY success to the registry —
                // a LATE one is audit-only for the DECISION (cell
                // unusable) but never dropped and never refetched.
                publishes.push((i, slots.boundary_secs_of_day));
                // The <= cutoff gate is the runner's finalize guard
                // mirror: a response completing PAST the cutoff never
                // contributes to a decision (audit-only).
                if completed_at <= slots.dhan_cutoff_ms {
                    chains_ok[i] = true;
                    decide_ready_at = decide_ready_at.max(completed_at);
                }
            }
            Some(err) => {
                if failure_arms_ladder(&err) {
                    arming = true;
                }
                // One gated retry admitted onto the retry grid, if it can
                // land — fired AFTER the remaining primaries (below).
                if next_retry_slot < slots.dhan_chain_retry_slots_ms.len()
                    && may_retry_in_cycle(
                        &err,
                        retries_used_chain[i],
                        cfg.in_cycle_retry_max,
                        slots.dhan_chain_retry_slots_ms[next_retry_slot],
                        CADENCE_RETRY_LATENCY_ALLOWANCE_MS,
                        slots.dhan_cutoff_ms,
                    )
                {
                    retries_used_chain[i] += 1;
                    retry_queue.push((
                        slots.dhan_chain_retry_slots_ms[next_retry_slot],
                        SimRetry::Chain { underlying_idx: i },
                    ));
                    next_retry_slot += 1;
                }
            }
        }
    }

    // Spot singles (nominal) — failures collected for the APPEND grid
    // (design §1: spot retries are appended AFTER the last nominal spot
    // single, never contending a nominal slot's gate window — the
    // runner's `next_spot_retry_target_ms` mirror).
    let mut spot_failures: Vec<(usize, CadenceFetchError)> = Vec::new();
    for k in 0..4 {
        let jitter = i64::try_from(rng.below(500)).unwrap_or(0);
        let fired_at = sim_gated_spot_fire(
            clock,
            gates,
            ledger,
            slots.dhan_spot_slots_ms[k],
            jitter,
            true,
            &mut dispatched_late,
            nominal_denials,
        );
        let latency = 10 + i64::try_from(rng.below(7_990)).unwrap_or(0);
        let outcome = SimOutcome::draw(rng, fail_bias_pct);
        match outcome.as_error() {
            None if fired_at + latency <= slots.dhan_cutoff_ms => {
                spots_ok[k] = true;
                if k < ChainUnderlying::COUNT {
                    decide_ready_at = decide_ready_at.max(fired_at + latency);
                }
            }
            None => {}
            Some(err) => {
                if failure_arms_ladder(&err) {
                    arming = true;
                }
                if matches!(err, CadenceFetchError::RateLimited { .. }) {
                    spot_dirty = true;
                }
                spot_failures.push((k, err));
            }
        }
    }
    let mut next_spot_retry_target = slots.dhan_spot_slots_ms[3] + CADENCE_SPOT_WINDOW_MS;
    for (k, err) in spot_failures {
        let retry_target = next_spot_retry_target.max(clock.wall_ms);
        if may_retry_in_cycle(
            &err,
            retries_used_spot[k],
            cfg.in_cycle_retry_max,
            retry_target,
            CADENCE_RETRY_LATENCY_ALLOWANCE_MS,
            slots.dhan_cutoff_ms,
        ) {
            retries_used_spot[k] += 1;
            next_spot_retry_target = retry_target + CADENCE_SPOT_WINDOW_MS;
            retry_queue.push((retry_target, SimRetry::Spot { target_idx: k }));
        }
    }

    // The merged retry grid, in target order — the runner's sorted event
    // queue interleaving (spot appends and chain grid slots can overlap;
    // the queue fires them by target instant).
    retry_queue.sort_by_key(|(target, _)| *target);
    for (target, retry) in retry_queue {
        match retry {
            SimRetry::Chain { underlying_idx } => {
                let jitter = i64::try_from(rng.below(500)).unwrap_or(0);
                let refired = sim_gated_chain_fire(
                    clock,
                    gates,
                    ledger,
                    ChainUnderlying::ALL[underlying_idx],
                    target,
                    jitter,
                    false,
                    &mut dispatched_late,
                    nominal_denials,
                );
                let latency = 10 + i64::try_from(rng.below(7_990)).unwrap_or(0);
                if SimOutcome::draw(rng, fail_bias_pct) == SimOutcome::Ok {
                    publishes.push((underlying_idx, slots.boundary_secs_of_day));
                    if refired + latency <= slots.dhan_cutoff_ms {
                        chains_ok[underlying_idx] = true;
                        decide_ready_at = decide_ready_at.max(refired + latency);
                    }
                }
            }
            SimRetry::Spot { target_idx } => {
                let refired = sim_gated_spot_fire(
                    clock,
                    gates,
                    ledger,
                    target,
                    0,
                    false,
                    &mut dispatched_late,
                    nominal_denials,
                );
                let latency = 10 + i64::try_from(rng.below(7_990)).unwrap_or(0);
                let outcome = SimOutcome::draw(rng, fail_bias_pct);
                if outcome == SimOutcome::RateLimited {
                    spot_dirty = true;
                }
                if outcome == SimOutcome::Ok && refired + latency <= slots.dhan_cutoff_ms {
                    spots_ok[target_idx] = true;
                    if target_idx < ChainUnderlying::COUNT {
                        decide_ready_at = decide_ready_at.max(refired + latency);
                    }
                }
            }
        }
    }

    // Decision: 3 chains + 3 underlying spots (VIX advisory — index 3).
    // MIRRORS the runner's finalize guard ("never a late decision",
    // runner.rs finalize_if_complete now≤cutoff): only responses
    // completing AT/BEFORE the cutoff contribute (the ok-flag gates
    // above), and the Decided emit instant is the ACTUAL completion
    // instant of the last required leg — never clamped to satisfy the
    // assertion. Weakening the mirror (dropping the <= cutoff gates)
    // fails the no-late-decision assert on late-completing permutations.
    let complete = chains_ok.iter().all(|c| *c) && spots_ok[..3].iter().all(|s| *s);
    let outcome = if complete {
        SimLaneOutcome {
            outcome: DecisionOutcome::Decided,
            emitted_at_wall: decide_ready_at,
        }
    } else {
        // Incomplete: the cutoff event emits the skip (possibly late —
        // a skip is allowed late; a DECIDED is not).
        SimLaneOutcome {
            outcome: DecisionOutcome::Skipped(SkipReason::Cutoff),
            emitted_at_wall: slots.dhan_cutoff_ms.max(clock.wall_ms),
        }
    };
    (arming, spot_dirty, outcome)
}

// ---------------------------------------------------------------------------
// THE PROOF
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// 64-cycle deterministic replays under skew/jitter/GC/latency/
    /// failure/restart permutations: ZERO rate violations, zero nominal
    /// denials, ≤1 decision per (lane, cycle) — the design §11 proof.
    #[test]
    fn proptest_cadence_replay_zero_rate_violations(
        seed in any::<u64>(),
        boot_skew in -2_000_i64..2_000,
        fail_bias_pct in 0_u64..60,
        restart_every in 0_usize..20,
    ) {
        let cfg = CadenceConfig::default();
        let mut rng = SimRng(seed);
        let mut clock = SimClock {
            wall_ms: 9 * 3_600_000 + 14 * 60_000, // 09:14:00 IST
            mono_origin: boot_skew,
        };
        let mut gates = DhanGates::new(cfg.chain_min_spacing_ms, cfg.spot_window_cap);
        gates.reseed_all(clock.mono());
        let mut ledger = FireLedger::default();
        let mut latch = DecisionLatch::new();
        let mut rung = 0_u8;
        // The 2026-07-15 adaptive ladders, folded EXACTLY as the runner
        // folds them — every step + transition drives real slot tables
        // through the real gates.
        let spot_step_floor = min_spot_step_for_cap(cfg.spot_window_cap);
        let mut spot_ladder = StreakLadder::starting_at(spot_step_floor);
        let mut groww_ladder = StreakLadder::starting_at(0);
        let mut last_boundary = None;
        let mut nominal_denials = 0_u32;
        let mut cycles = 0_u32;
        let mut spot_steps_seen = [false; 4];
        let mut decisions_per_cycle: Vec<u32> = Vec::new();
        let mut publishes: Vec<(usize, u32)> = Vec::new();
        // Boot = a reseed: the first cycle demotes nominal deferrals (the
        // runner's `demote_nominal` mirror).
        let mut post_reseed = true;

        while cycles < 64 {
            // Occasional GC pause between cycles (advances real time).
            if rng.below(4) == 0 {
                let pause = i64::try_from(rng.below(3_000)).unwrap_or(0);
                clock.advance_to_wall(clock.wall_ms + pause);
            }
            // Random process restart: fresh gates, conservative reseed,
            // no-mid-cycle-join boundary selection.
            if restart_every > 0 && cycles as usize % restart_every == restart_every - 1 {
                let skew = i64::try_from(rng.below(4_000)).unwrap_or(0) - 2_000;
                clock.restart(skew);
                gates = DhanGates::new(cfg.chain_min_spacing_ms, cfg.spot_window_cap);
                gates.reseed_all(clock.mono());
                post_reseed = true;
            }
            let Some(boundary) =
                next_joinable_boundary(clock.wall_ms, last_boundary, rung, &cfg)
            else {
                break; // session window exhausted for the day
            };
            last_boundary = Some(boundary);
            let slots = build_cycle_slots(boundary, rung, spot_ladder.step, groww_ladder.step, &cfg);
            spot_steps_seen[usize::from(spot_ladder.step.min(3))] = true;
            // No-overlap invariant (coordinator 2026-07-15): even the
            // WORST Groww shape's last wave + verdict + a full sequential
            // fallback tail can never reach the next minute's :00 burst.
            prop_assert!(
                slots.groww_verdict_ms
                    + 7 * cfg.groww_request_timeout_ms
                    <= slots.boundary_ms + 60_000,
                "groww fallback tail overlaps the next :00 burst (shape {})",
                slots.groww_shape
            );

            let (arming, spot_dirty, dhan) = sim_dhan_cycle(
                &mut clock,
                &gates,
                &mut ledger,
                &slots,
                &cfg,
                &mut rng,
                fail_bias_pct,
                &mut nominal_denials,
                post_reseed,
                &mut publishes,
            );
            post_reseed = false;
            // Fold the adaptive ladders exactly as the runner does: the
            // Dhan spot ladder on REAL sim dirt; the Groww shape ladder
            // on a scripted dirty draw (the sim fires no Groww requests
            // — the SHAPE permutations are what must drive the slot
            // table + no-overlap invariant above).
            let _ = spot_ladder.advance(
                spot_dirty,
                cfg.concurrency_degrade_after_dirty_cycles,
                cfg.concurrency_recover_after_clean_cycles,
                spot_step_floor,
                SPOT_CONCURRENCY_MAX_STEP,
            );
            let groww_dirty = rng.below(100) < fail_bias_pct;
            let _ = groww_ladder.advance(
                groww_dirty,
                cfg.concurrency_degrade_after_dirty_cycles,
                cfg.concurrency_recover_after_clean_cycles,
                0,
                GROWW_SHAPE_MAX_STEP,
            );

            // Exactly-once decision latch per (lane, cycle); a DECIDED
            // outcome is never emitted past the lane cutoff (the sim's
            // emit instant is the ACTUAL last-required-completion wall
            // instant — see sim_dhan_cycle; a skip MAY be emitted late);
            // every skip carries a typed reason.
            let mut emitted = 0_u32;
            if latch.try_latch(Feed::Dhan, slots.cycle_minute_ist) {
                emitted += 1;
                match dhan.outcome {
                    DecisionOutcome::Decided | DecisionOutcome::DecidedDegraded => {
                        prop_assert!(
                            dhan.emitted_at_wall <= slots.dhan_cutoff_ms,
                            "a DECIDED outcome must never be emitted past the lane cutoff \
                             (emitted {} > cutoff {})",
                            dhan.emitted_at_wall,
                            slots.dhan_cutoff_ms
                        );
                    }
                    DecisionOutcome::Skipped(reason) => {
                        prop_assert!(!reason.as_str().is_empty());
                    }
                }
            }
            prop_assert!(!latch.try_latch(Feed::Dhan, slots.cycle_minute_ist));
            // The Groww lane is gate-free BY CONSTRUCTION (no Groww arm
            // in this sim touches DhanGates); its latch is independent.
            if latch.try_latch(Feed::Groww, slots.cycle_minute_ist) {
                emitted += 1;
            }
            decisions_per_cycle.push(emitted);

            // The ladder walks on arming failures, recovers on clean.
            let verdict = if arming {
                CycleVerdict::DhanFailed
            } else {
                CycleVerdict::Clean
            };
            rung = next_rung(rung, verdict, cfg.dhan_ladder_max_rungs);
            prop_assert!(rung <= cfg.dhan_ladder_max_rungs);
            cycles += 1;
            // Move past the cycle tail before the next boundary pick.
            clock.advance_to_wall(slots.dhan_cutoff_ms);
        }

        // MINIMUM-ACTIVITY floor (Rule 11 — the proof must never pass
        // vacuously): all 64 cycles ran, and every cycle fired its 3
        // chain primaries + 4 spot singles at minimum. A regression in
        // the boundary/joinable calculus or the window constants that
        // silently produced zero (or partial) activity fails HERE.
        prop_assert_eq!(cycles, 64, "the replay must complete all 64 cycles");
        prop_assert!(
            ledger.chain_global.len() >= 3 * 64,
            "chain fires below the 3-primaries-per-cycle floor: {}",
            ledger.chain_global.len()
        );
        prop_assert!(
            ledger.spot.len() >= 4 * 64,
            "spot fires below the 4-singles-per-cycle floor: {}",
            ledger.spot.len()
        );
        // THE floors: per-UL ≥3000, GLOBAL ≥3000, spot ≤ window cap per
        // rolling 1000ms — including retries, ladder steps/transitions,
        // rungs and restarts, in the broker wall domain.
        ledger.assert_floors(cfg.chain_min_spacing_ms, cfg.spot_window_cap, "replay");
        prop_assert_eq!(nominal_denials, 0, "gate denials on nominal slots");
        // The concurrency ladder starts at the cap's structural floor
        // (step 0 for the default cap 4) — the full-parallel grouping is
        // ALWAYS exercised, never a vacuous degraded-only run.
        prop_assert!(spot_steps_seen[usize::from(spot_step_floor)]);
        // Exactly ONE decision per lane per cycle: the latch must ADMIT
        // every fresh (lane, minute) pair (a wrongly-refusing latch reads
        // < 2) — the ≤1 half is the immediate re-latch refusals above.
        for (i, d) in decisions_per_cycle.iter().enumerate() {
            prop_assert_eq!(*d, 2, "cycle {}: both lanes must emit exactly once", i);
        }
        // Snapshot published EXACTLY once per successful chain fetch per
        // (underlying, cycle): a success — even a LATE one — is never
        // refetched (the retry policy fires only on Err), so no
        // (underlying, boundary) pair may publish twice; late successes
        // still published (never dropped).
        let mut seen_publishes = std::collections::HashSet::new();
        for p in &publishes {
            prop_assert!(
                seen_publishes.insert(*p),
                "chain snapshot published more than once for (underlying, boundary) {:?}",
                p
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Deterministic named races (design §11)
// ---------------------------------------------------------------------------

#[test]
fn test_minute_boundary_race_no_double_fire() {
    // Wakes straddling a boundary T±ε can never select the same boundary
    // twice: the strictly-after-last horizon makes an instant-completing
    // cycle unable to re-latch its own boundary.
    let cfg = CadenceConfig::default();
    let t = 36_000; // 10:00:00
    let t_ms = i64::from(t) * 1_000;
    // Wake 1ms BEFORE T with the previous boundary already completed —
    // T's pre-fires began long ago (T−5000) → T is NOT joinable; skip.
    assert_eq!(
        next_joinable_boundary(t_ms - 1, Some(t - 60), 0, &cfg),
        Some(t + 60)
    );
    // Wake 1ms AFTER T, same story.
    assert_eq!(
        next_joinable_boundary(t_ms + 1, Some(t - 60), 0, &cfg),
        Some(t + 60)
    );
    // An instant-completing cycle AT T never re-selects T.
    assert_eq!(next_joinable_boundary(t_ms, Some(t), 0, &cfg), Some(t + 60));
    // A double latch of the same (lane, minute) is refused.
    let mut latch = DecisionLatch::new();
    assert!(latch.try_latch(Feed::Dhan, t - 60));
    assert!(!latch.try_latch(Feed::Dhan, t - 60));
}

#[test]
fn test_restart_mid_cycle_cannot_violate_spacing() {
    // Old process fires NIFTY at :55 and DIES 100ms later. The new
    // process cannot know that fire (fresh monotonic domain) — the
    // conservative reseed + no-mid-cycle-join still hold the floor.
    let cfg = CadenceConfig::default();
    let t_ms = 36_000_000_i64; // 10:00:00
    let mut clock = SimClock {
        wall_ms: t_ms - 5_000,
        mono_origin: 0,
    };
    let mut ledger = FireLedger::default();
    let gates = DhanGates::new(cfg.chain_min_spacing_ms, cfg.spot_window_cap);
    gates.reseed_all(clock.mono() - cfg.chain_min_spacing_ms); // long-running: gates clear
    let mut late = false;
    let mut denials = 0;
    sim_gated_chain_fire(
        &mut clock,
        &gates,
        &mut ledger,
        ChainUnderlying::Nifty,
        t_ms - 5_000,
        0,
        true,
        &mut late,
        &mut denials,
    );
    // CRASH + instant reboot 100ms later: fresh gates, conservative reseed.
    clock.advance_to_wall(t_ms - 4_900);
    clock.restart(-1_234);
    let gates = DhanGates::new(cfg.chain_min_spacing_ms, cfg.spot_window_cap);
    gates.reseed_all(clock.mono());
    // The new process tries to fire NIFTY IMMEDIATELY (a hostile joiner —
    // the real runner would not even select this boundary, per
    // no-mid-cycle-join, asserted next).
    assert_eq!(
        next_joinable_boundary(clock.wall_ms, None, 0, &cfg),
        Some(36_060),
        "no-mid-cycle-join skips the in-flight cycle"
    );
    let hostile_target = clock.wall_ms;
    sim_gated_chain_fire(
        &mut clock,
        &gates,
        &mut ledger,
        ChainUnderlying::Nifty,
        hostile_target,
        0,
        false,
        &mut late,
        &mut denials,
    );
    // Broker-side (wall) delta between the two processes' NIFTY fires
    // still ≥ 3000 — reseed pushed the new fire to boot + spacing.
    ledger.assert_floors(cfg.chain_min_spacing_ms, cfg.spot_window_cap, "restart");
    let nifty = &ledger.per_underlying[ChainUnderlying::Nifty.index()];
    assert_eq!(nifty.len(), 2);
    assert!(nifty[1] - nifty[0] >= cfg.chain_min_spacing_ms);
}

#[test]
fn test_retry_through_gate_never_compresses_chain_spacing() {
    // A failed :55 NIFTY primary retried on the retry grid (after the
    // :02 SENSEX primary) passes BOTH its per-UL gate and the GLOBAL
    // gate — the merged fire sequence keeps every delta ≥ 3000ms even
    // when the retry target lands hostile-early.
    let cfg = CadenceConfig::default();
    let t = 36_000_u32;
    let slots = build_cycle_slots(t, 0, 0, 0, &cfg);
    let mut clock = SimClock {
        wall_ms: slots.dhan_chain_slots_ms[0] - 10_000,
        mono_origin: 0,
    };
    let gates = DhanGates::new(cfg.chain_min_spacing_ms, cfg.spot_window_cap);
    gates.reseed_all(clock.mono());
    let mut ledger = FireLedger::default();
    let mut late = false;
    let mut denials = 0;
    // The 3 primaries fire on schedule.
    for (i, u) in ChainUnderlying::ALL.iter().enumerate() {
        sim_gated_chain_fire(
            &mut clock,
            &gates,
            &mut ledger,
            *u,
            slots.dhan_chain_slots_ms[i],
            0,
            true,
            &mut late,
            &mut denials,
        );
    }
    assert_eq!(denials, 0, "nominal primaries never gate-deferred");
    // NIFTY retry attempted HOSTILE-EARLY (1ms after SENSEX) — the gates
    // defer it; the retry-policy check also proves the grid slot lands.
    let err = CadenceFetchError::Transport;
    assert!(may_retry_in_cycle(
        &err,
        0,
        cfg.in_cycle_retry_max,
        slots.dhan_chain_retry_slots_ms[0],
        CADENCE_RETRY_LATENCY_ALLOWANCE_MS,
        slots.dhan_cutoff_ms,
    ));
    sim_gated_chain_fire(
        &mut clock,
        &gates,
        &mut ledger,
        ChainUnderlying::Nifty,
        slots.dhan_chain_slots_ms[2] + 1,
        0,
        false,
        &mut late,
        &mut denials,
    );
    ledger.assert_floors(cfg.chain_min_spacing_ms, cfg.spot_window_cap, "retry");
    assert_eq!(ledger.chain_global.len(), 4);
}

// ---------------------------------------------------------------------------
// Boundary tests — #1540 consumption (moneyness + chain_snapshot)
// ---------------------------------------------------------------------------
// Each registry-touching test uses a DISTINCT (feed, underlying) slot —
// the registry is process-global; distinct slots keep parallel test
// threads interference-free.

#[test]
fn test_cadence_atm_exact_strike_is_atm() {
    // Spot exactly ON the strike grid: ATM == spot; a CE at that strike
    // classifies Atm via the #1540 integer math the assembly consumes.
    let mut a = LaneAssembly::new(Feed::Dhan, 35_940, 36_000_000);
    a.record_spot(
        SpotTarget::Nifty,
        24_500.0, // NIFTY step 50.00 — exactly on-grid
        SpotProvenance::OwnFetch,
        36_003_000,
        35_940,
    );
    let cell = a.spot(ChainUnderlying::Nifty).copied();
    assert!(cell.is_some_and(|c| c.spot_paise == 2_450_000 && c.atm_paise == 2_450_000));
}

#[test]
fn test_cadence_atm_midpoint_tie_rounds_up() {
    // Spot exactly midway between two strikes (24_525.00 with step
    // 50.00) — the #1540 grid rule rounds the tie UP.
    let mut a = LaneAssembly::new(Feed::Dhan, 35_940, 36_000_000);
    a.record_spot(
        SpotTarget::Nifty,
        24_525.0,
        SpotProvenance::OwnFetch,
        36_003_000,
        35_940,
    );
    let cell = a.spot(ChainUnderlying::Nifty).copied();
    assert!(
        cell.is_some_and(|c| c.atm_paise == 2_455_000),
        "tie rounds UP"
    );
}

#[test]
fn test_cadence_invalid_spot_all_unknown_surfaced() {
    // Slot: (Dhan, Banknifty). A published chain + an INVALID spot anchor
    // (0 paise) → every row folds Unknown → all_unknown() is TRUE and the
    // decision layer skips AllUnknown — surfaced, never silently dropped.
    publish_chain_snapshot(ChainMoneynessSnapshot {
        feed: Feed::Dhan,
        underlying: ChainUnderlying::Banknifty,
        minute_ts_ist_nanos: 1,
        fetched_at_ist_nanos: 2,
        underlying_spot: 0.0,
        underlying_spot_paise: 0,
        atm_strike_paise: 0,
        expiry_ist_nanos: 0,
        spot_missing: true,
        rows: vec![
            SnapshotRow {
                strike_paise: 5_100_000,
                ltp_paise: 100,
                leg: OptionLeg::Ce,
                moneyness: Moneyness::Unknown,
            },
            SnapshotRow {
                strike_paise: 5_110_000,
                ltp_paise: 90,
                leg: OptionLeg::Pe,
                moneyness: Moneyness::Unknown,
            },
        ],
    });
    let fold = fold_chain_moneyness(Feed::Dhan, ChainUnderlying::Banknifty, 0, 0);
    assert_eq!(fold.rows, 2);
    assert_eq!(fold.unknown, 2);
    assert!(fold.all_unknown(), "invalid spot ⇒ unusable, surfaced");
    assert_eq!(SkipReason::AllUnknown.as_str(), "all_unknown");
}

#[test]
fn test_cadence_empty_chain_sentinel_skips() {
    // Slot: (Dhan, Nifty) — NEVER published in this binary: the boot
    // sentinel folds to 0 rows ⇒ all_unknown ⇒ the decision path skips.
    let snap = load_chain_snapshot(Feed::Dhan, ChainUnderlying::Nifty);
    assert!(snap.is_empty_sentinel(), "pre-first-publish boot sentinel");
    let fold = fold_chain_moneyness(Feed::Dhan, ChainUnderlying::Nifty, 2_450_000, 2_450_000);
    assert_eq!(fold.rows, 0);
    assert!(fold.all_unknown(), "0 rows = unusable for the minute");
}

#[test]
fn test_cadence_200_empty_spot_fallback_chain_end_to_end() {
    // Slot: (Groww, Sensex). The lane's own spot fetch came back
    // 200-empty; the chain fetch succeeded WITH an embedded underlying
    // spot AND published to the registry. End-to-end: rung-3 fallback
    // fills the spot (provenance ChainEmbedded, degraded stamped), the
    // anchor resolves on the #1540 grid, and the fold classifies the
    // published rows against it.
    let embedded_spot = 81_002.0; // SENSEX step 100.00 → ATM 81_000.00
    publish_chain_snapshot(ChainMoneynessSnapshot {
        feed: Feed::Groww,
        underlying: ChainUnderlying::Sensex,
        minute_ts_ist_nanos: 10,
        fetched_at_ist_nanos: 20,
        underlying_spot: embedded_spot,
        underlying_spot_paise: 8_100_200,
        atm_strike_paise: 8_100_000,
        expiry_ist_nanos: 0,
        spot_missing: false,
        rows: vec![
            SnapshotRow {
                strike_paise: 8_100_000, // == ATM → Atm
                ltp_paise: 40_000,
                leg: OptionLeg::Ce,
                moneyness: Moneyness::Atm,
            },
            SnapshotRow {
                strike_paise: 8_000_000, // CE strike < spot → Itm
                ltp_paise: 110_000,
                leg: OptionLeg::Ce,
                moneyness: Moneyness::Itm,
            },
            SnapshotRow {
                strike_paise: 8_200_000, // CE strike > spot → Otm
                ltp_paise: 5_000,
                leg: OptionLeg::Ce,
                moneyness: Moneyness::Otm,
            },
        ],
    });
    let mut a = LaneAssembly::new(Feed::Groww, 35_940, 36_000_000);
    a.record_chain(
        ChainUnderlying::Sensex,
        ChainCell {
            provenance: ChainProvenance::OwnFetch,
            source_feed: Feed::Groww,
            published_to_registry: true,
            fetched_at_ms: 36_000_400,
            minute_ist: 35_940,
            embedded_spot: Some(embedded_spot),
        },
    );
    // Own spot: 200-empty → never recorded. Rung 3 fills from the chain.
    assert!(a.spot(ChainUnderlying::Sensex).is_none());
    assert_eq!(a.fill_spots_from_chain_embedded(36_000_900), 1);
    let cell = a.spot(ChainUnderlying::Sensex).copied();
    assert!(
        cell.is_some_and(|c| c.provenance == SpotProvenance::ChainEmbedded
            && c.spot_paise == 8_100_200
            && c.atm_paise == 8_100_000)
    );
    assert!(a.any_degraded_provenance(), "fallback stamps degraded");
    // The fold against the resolved anchor sees the published rows.
    let anchor = cell.map(|c| (c.spot_paise, c.atm_paise));
    let fold =
        anchor.map(|(s, m)| fold_chain_moneyness(Feed::Groww, ChainUnderlying::Sensex, s, m));
    assert!(fold.is_some_and(|f| f.rows == 3
        && f.atm == 1
        && f.itm == 1
        && f.otm == 1
        && !f.all_unknown()));
}

#[test]
fn test_cadence_guarded_fold_reads_source_feed_slot_and_rejects_stale_minute() {
    // Slot: (Groww, Banknifty). A Dhan-lane CROSS-FILLED cell must fold
    // the LENDER's (Groww) registry slot — the borrowed chain's rows
    // never live under the borrower's slot (design §3(e): "Groww's
    // same-cycle data drives the Dhan lane") — and a snapshot from the
    // WRONG minute folds to 0 rows (all_unknown, SURFACED) per the
    // design §6 stale-registry guard.
    let minute: u32 = 35_940; // 09:59:00 — the decided minute
    let day: i64 = 20_000; // an arbitrary IST calendar day
    let minute_nanos = (day * 86_400 + i64::from(minute)) * 1_000_000_000;
    let now_nanos = minute_nanos + 61 * 1_000_000_000; // ~T+1s of the cycle
    publish_chain_snapshot(ChainMoneynessSnapshot {
        feed: Feed::Groww,
        underlying: ChainUnderlying::Banknifty,
        minute_ts_ist_nanos: minute_nanos,
        fetched_at_ist_nanos: minute_nanos + 400_000_000,
        underlying_spot: 51_000.0,
        underlying_spot_paise: 5_100_000,
        atm_strike_paise: 5_100_000,
        expiry_ist_nanos: 0,
        spot_missing: false,
        rows: vec![
            SnapshotRow {
                strike_paise: 5_100_000, // == ATM → Atm
                ltp_paise: 100,
                leg: OptionLeg::Ce,
                moneyness: Moneyness::Atm,
            },
            SnapshotRow {
                strike_paise: 5_000_000, // CE strike < spot → Itm
                ltp_paise: 200,
                leg: OptionLeg::Ce,
                moneyness: Moneyness::Itm,
            },
        ],
    });
    // The Dhan lane's cross-filled cell KEEPS the lender's identity.
    let cell = ChainCell {
        provenance: ChainProvenance::CrossSource,
        source_feed: Feed::Groww,
        published_to_registry: true,
        fetched_at_ms: 36_000_400,
        minute_ist: minute,
        embedded_spot: None,
    };
    let fold = fold_chain_cell_moneyness(
        &cell,
        ChainUnderlying::Banknifty,
        minute,
        now_nanos,
        5_100_000,
        5_100_000,
    );
    assert_eq!(fold.rows, 2, "the lender's fresh rows drive the fold");
    assert!(!fold.all_unknown());
    // The WRONG cycle minute (a previous-minute registry residue) is
    // REFUSED — 0 rows, all_unknown, surfaced; never a silent stale-row
    // classification against the current minute's anchors.
    let stale = fold_chain_cell_moneyness(
        &cell,
        ChainUnderlying::Banknifty,
        minute - 60,
        now_nanos,
        5_100_000,
        5_100_000,
    );
    assert_eq!(stale.rows, 0);
    assert!(stale.all_unknown(), "stale snapshot = unusable, surfaced");
}
