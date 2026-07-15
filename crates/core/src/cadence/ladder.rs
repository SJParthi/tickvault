//! The Dhan failure ladder (design §3) + the in-cycle retry policy +
//! the ADAPTIVE CONCURRENCY ladders (operator spec addition 2026-07-15).
//!
//! The ANCHOR ladder shifts the NEXT cycle's Dhan anchor 1s earlier per
//! failing cycle (:55 → :54 → … → :50 floor at rung 5 — you cannot rewind
//! time inside a cycle) and steps back ONE rung per fully-clean cycle
//! (hysteresis — a reset-to-:55 would oscillate full-amplitude against a
//! flapping vendor; unanimous designer choice, Assumed pending operator
//! confirm). The rung resets to 0 at day start / process boot (the ladder
//! is a day-scoped in-memory variable, NOT a cycle FSM state).
//!
//! The CONCURRENCY ladders (2026-07-15, [`StreakLadder`]) are streak-driven:
//! - **Dhan spot concurrency** — steps encode the spot GROUPINGS
//!   `[[4]] → [[3],[1]] → [[2],[2]] → [[1],[1],[1],[1]]` (groups fire at
//!   consecutive 1000ms-spaced anchors from the spot anchor slot).
//! - **Groww fallback shape** — the coordinator-relayed three-choice
//!   ladder (2026-07-15): choice 1 = `:00` all-7-parallel; choice 2 =
//!   `:01` chains / `:02` ALL 4 spots; choice 3 (last resort) = `:01`
//!   chains / `:02` core spots / `:03` VIX alone.
//!
//! Both degrade ONE step after `concurrency_degrade_after_dirty_cycles`
//! (default 2, Assumed — flagged for operator confirm) CONSECUTIVE
//! rate-limited-dirty cycles and recover one step after
//! `concurrency_recover_after_clean_cycles` (default 3, Assumed — flagged
//! for operator confirm) consecutive fully-clean cycles — never
//! permanently stuck from a one-off 429. Day-scoped like the anchor rung.

use super::executor::CadenceFetchError;

/// Highest Dhan spot-concurrency step — step 3 = fully sequential 1×4.
pub const SPOT_CONCURRENCY_MAX_STEP: u8 = 3;

/// Highest Groww fallback-shape step — step 2 = the three-second split
/// (choice 3, the LAST resort).
pub const GROWW_SHAPE_MAX_STEP: u8 = 2;

/// A streak-driven adaptive ladder (shared primitive of the Dhan
/// spot-concurrency ladder and the Groww fallback-shape ladder): degrade
/// ONE step after N consecutive dirty cycles, recover one step after M
/// consecutive clean cycles. Steps are clamped to `[min_step, max_step]`
/// at every transition.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StreakLadder {
    /// Current step (0 = the most concurrent shape).
    pub step: u8,
    /// Consecutive dirty cycles at the current step.
    dirty_streak: u32,
    /// Consecutive fully-clean cycles at the current step.
    clean_streak: u32,
}

/// A [`StreakLadder::advance`] transition (None = no step change).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreakShift {
    /// Stepped DOWN the shape ladder (step + 1, less concurrent).
    Degraded,
    /// Stepped back UP (step − 1, more concurrent).
    Recovered,
}

impl StreakLadder {
    /// A ladder starting at `min_step` (the structural floor — e.g. a
    /// `spot_window_cap` below the full group size floors the Dhan spot
    /// step via [`min_spot_step_for_cap`]).
    #[must_use]
    // TEST-EXEMPT: covered by the StreakLadder unit tests (floor-start arms; guard name-pattern mismatch).
    pub fn starting_at(min_step: u8) -> Self {
        Self {
            step: min_step,
            dirty_streak: 0,
            clean_streak: 0,
        }
    }

    /// Fold one whole-cycle verdict: `dirty` = ≥1 rate-limited outcome on
    /// the ladder's legs this cycle. Degrades one step after
    /// `degrade_after` CONSECUTIVE dirty cycles ("continuous", never a
    /// one-off); recovers one step after `recover_after` consecutive
    /// fully-clean cycles. A dirty cycle resets the clean streak and vice
    /// versa; a shift resets both streaks (the next shift needs a fresh
    /// full streak at the NEW step). Pure state fold — no I/O.
    pub fn advance(
        &mut self,
        dirty: bool,
        degrade_after: u32,
        recover_after: u32,
        min_step: u8,
        max_step: u8,
    ) -> Option<StreakShift> {
        // Defensive clamp (config floors can only move between boots; the
        // in-memory step must never sit outside the legal band).
        self.step = self.step.clamp(min_step, max_step);
        if dirty {
            self.clean_streak = 0;
            self.dirty_streak = self.dirty_streak.saturating_add(1);
            if self.dirty_streak >= degrade_after && self.step < max_step {
                self.step = self.step.saturating_add(1);
                self.dirty_streak = 0;
                return Some(StreakShift::Degraded);
            }
            None
        } else {
            self.dirty_streak = 0;
            self.clean_streak = self.clean_streak.saturating_add(1);
            if self.clean_streak >= recover_after && self.step > min_step {
                self.step = self.step.saturating_sub(1);
                self.clean_streak = 0;
                return Some(StreakShift::Recovered);
            }
            None
        }
    }
}

/// The Dhan spot-concurrency GROUPING: which 1000ms-spaced group anchor
/// (0-based, from the spot anchor slot) spot target `target_idx`
/// (`SpotTarget::ALL` order: NIFTY/BANKNIFTY/SENSEX/INDIA VIX) fires at,
/// per ladder step. Steps: 0 = `[[4]]`, 1 = `[[3],[1]]` (VIX alone
/// second), 2 = `[[2],[2]]`, 3 = `[[1],[1],[1],[1]]`. Steps past the max
/// clamp to fully sequential. Pure.
#[must_use]
pub fn spot_group_index(step: u8, target_idx: usize) -> usize {
    match step {
        0 => 0,
        1 => usize::from(target_idx >= 3),
        2 => target_idx / 2,
        _ => target_idx,
    }
}

/// The STRUCTURAL concurrency-step floor for a configured
/// `spot_window_cap`: no step-`s` group may exceed the rolling-window cap
/// (a cap of 4..=5 admits the full step-0 simultaneous group; 3 → step 1;
/// 2 → step 2; 1 → fully sequential). Pure.
#[must_use]
// TEST-EXEMPT: covered by the spot-cap floor arms of the StreakLadder unit tests (guard name-pattern mismatch).
pub fn min_spot_step_for_cap(cap: u32) -> u8 {
    match cap {
        0 | 1 => 3, // 0 is unreachable post-validate; fail-closed sequential
        2 => 2,
        3 => 1,
        _ => 0,
    }
}

/// The Groww fallback-shape WAVE indices `(chains, core_spots, vix_spot)`
/// — each a 0-based multiple of `CADENCE_GROWW_WAVE_STEP_MS` past the
/// burst anchor. Choice 1 (step 0): all at `:00`; choice 2 (step 1):
/// chains `:01`, ALL 4 spots `:02`; choice 3 (step 2, last resort):
/// chains `:01`, core spots `:02`, VIX alone `:03` (deliberately lowest
/// priority — context-only). Steps past the max clamp to choice 3. Pure.
#[must_use]
pub const fn groww_wave_indices(shape: u8) -> (usize, usize, usize) {
    match shape {
        0 => (0, 0, 0),
        1 => (1, 2, 2),
        _ => (1, 2, 3),
    }
}

/// The per-day Dhan ladder state (design §7: separate from the cycle FSM).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LadderState {
    /// Current rung, 0..=`dhan_ladder_max_rungs` (0 = nominal :55 anchor).
    pub rung: u8,
}

/// The whole-cycle verdict feeding the ladder transition.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CycleVerdict {
    /// No arming Dhan failure this cycle → recover one rung toward 0.
    Clean,
    /// ≥1 arming Dhan failure this cycle → shift one rung toward the floor.
    DhanFailed,
    /// The floor rung failed again → hold at the floor (cross-source
    /// steady state; exits on the first clean Dhan cycle).
    FloorExhausted,
}

/// Pure ladder transition: `Clean → −1 (min 0)`, `DhanFailed → +1
/// (max `max_rungs`)`, `FloorExhausted → hold max_rungs` (design §7).
#[must_use]
pub fn next_rung(cur: u8, verdict: CycleVerdict, max_rungs: u8) -> u8 {
    match verdict {
        CycleVerdict::Clean => cur.saturating_sub(1),
        CycleVerdict::DhanFailed => cur.saturating_add(1).min(max_rungs),
        CycleVerdict::FloorExhausted => max_rungs,
    }
}

/// Does this fetch failure ARM the ladder? (design §3(c), Assumed —
/// flagged for operator confirm.)
///
/// - `RateLimited` (a 429 — the strongest arming signal), `Timeout` and
///   `Transport` (5xx folds into `Transport` at the executor seam) ARM it
///   — "both buckets" per the operator's rule for genuine failures.
/// - `Empty` (Dhan spot 200-with-zero-candles) does NOT arm it: an
///   earlier start moves the request EARLIER relative to close, REDUCING
///   serving-lag headroom — arming on the 14-day 200-empty saga would
///   drive the schedule the wrong direction every minute (judge ruling,
///   design §0 — flagged Assumed deviation).
/// - `Auth` / `Malformed` do NOT arm it: neither is a pacing/serving-lag
///   signal (shifting the schedule earlier cannot fix a dead token or a
///   schema drift); both still degrade the lane loudly via CADENCE-01.
/// - `QueueDelay` does NOT arm it (verifier F1(iii), dated 2026-07-15): a
///   shared-limiter queue deadline miss is SELF-INFLICTED pacing — the
///   scheduler's own composition with the `dhan_data_api_limiter` — not a
///   vendor signal; arming on it would let our own defense-in-depth
///   limiter walk the anchor earlier forever.
///
/// STRENGTHENED ASSUMED FLAG (verifier F7, dated 2026-07-15): the
/// operator's verbatim rule arms the ladder on ANY arming failure in the
/// cycle EVEN WHEN an in-cycle retry recovered the leg. Under a perfectly
/// alternating fail/clean minute pattern this yields a permanent
/// AMPLITUDE-1 rung oscillation (0 ↔ 1) — accepted as the CURRENT
/// CONTRACT (pinned by
/// `test_ladder_any_failure_arming_amplitude_1_oscillation`), flagged
/// Assumed pending operator confirmation of a "recovered-in-cycle does
/// not arm" refinement.
#[must_use]
pub fn failure_arms_ladder(err: &CadenceFetchError) -> bool {
    matches!(
        err,
        CadenceFetchError::RateLimited { .. }
            | CadenceFetchError::Timeout
            | CadenceFetchError::Transport
    )
}

/// May a failed request be retried IN-CYCLE? (design §3(b), Assumed.)
///
/// - A `RateLimited` is NEVER retried blind in-cycle — it kills the leg
///   for the cycle and arms the ladder.
/// - At most `retry_max` retries per failed request.
/// - The retry must LAND: the gate's earliest-allowed fire instant plus
///   the p95 latency allowance must sit at/before the lane's cutoff —
///   otherwise the retry could only produce a late (discarded) response.
#[must_use]
pub fn may_retry_in_cycle(
    err: &CadenceFetchError,
    retries_used: u32,
    retry_max: u32,
    earliest_fire_ms: i64,
    latency_allowance_ms: i64,
    lane_cutoff_abs_ms: i64,
) -> bool {
    if matches!(err, CadenceFetchError::RateLimited { .. }) {
        return false;
    }
    if retries_used >= retry_max {
        return false;
    }
    earliest_fire_ms.saturating_add(latency_allowance_ms) <= lane_cutoff_abs_ms
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ladder_walks_55_to_50_and_recovers_one_per_clean_cycle() {
        let max = 5;
        // Failing cycles walk :55 → :54 → :53 → :52 → :51 → :50 (rung 5).
        let mut rung = 0;
        for expected in 1..=5u8 {
            rung = next_rung(rung, CycleVerdict::DhanFailed, max);
            assert_eq!(rung, expected);
        }
        // Failing AT the floor holds the floor.
        assert_eq!(next_rung(rung, CycleVerdict::DhanFailed, max), 5);
        assert_eq!(next_rung(rung, CycleVerdict::FloorExhausted, max), 5);
        // Recovery steps back ONE rung per fully-clean cycle (hysteresis)
        // and converges home in ≤5 clean minutes.
        for expected in (0..=4u8).rev() {
            rung = next_rung(rung, CycleVerdict::Clean, max);
            assert_eq!(rung, expected);
        }
        // Clean at rung 0 stays home (saturating, never underflows).
        assert_eq!(next_rung(0, CycleVerdict::Clean, max), 0);
    }

    #[test]
    fn test_ladder_429_arms_immediately_never_blind_retry() {
        let rate_limited = CadenceFetchError::RateLimited {
            retry_after_ms: Some(2_000),
        };
        // The strongest arming signal…
        assert!(failure_arms_ladder(&rate_limited));
        // …and NEVER retried in-cycle, regardless of budget or room.
        assert!(!may_retry_in_cycle(
            &rate_limited,
            0,
            1,
            5_000,
            1_500,
            60_000
        ));
        // Timeout / Transport (incl. 5xx at the seam) also arm.
        assert!(failure_arms_ladder(&CadenceFetchError::Timeout));
        assert!(failure_arms_ladder(&CadenceFetchError::Transport));
        // Auth / Malformed degrade loudly but do not shift the schedule.
        assert!(!failure_arms_ladder(&CadenceFetchError::Auth));
        assert!(!failure_arms_ladder(&CadenceFetchError::Malformed));
    }

    #[test]
    fn test_ladder_spot_200_empty_does_not_arm() {
        // Assumed-rule pin (design §0 flagged deviation): a 200-empty spot
        // must NOT walk the anchor earlier — that reduces serving-lag
        // headroom in the very saga it would react to.
        assert!(!failure_arms_ladder(&CadenceFetchError::Empty));
        // An Empty IS still retryable in-cycle (one gated attempt).
        assert!(may_retry_in_cycle(
            &CadenceFetchError::Empty,
            0,
            1,
            5_000,
            1_500,
            15_000
        ));
    }

    #[test]
    fn test_may_retry_in_cycle_respects_gate_and_cutoff() {
        let e = CadenceFetchError::Transport;
        // Budget exhausted → no retry.
        assert!(!may_retry_in_cycle(&e, 1, 1, 5_000, 1_500, 60_000));
        // Gate's earliest instant + latency allowance past the cutoff →
        // the retry could only be a late response → refused.
        assert!(!may_retry_in_cycle(&e, 0, 1, 14_000, 1_500, 15_000));
        // Landing exactly AT the cutoff is admitted (inclusive).
        assert!(may_retry_in_cycle(&e, 0, 1, 13_500, 1_500, 15_000));
        // Room + budget → retry through the gates.
        assert!(may_retry_in_cycle(&e, 0, 1, 5_000, 1_500, 15_000));
    }

    #[test]
    fn test_cadence_ladder_next_rung_clamps_and_failure_arms_ladder_is_total() {
        // next_rung respects a NON-default max (config can lower the
        // ceiling below 5): the clamp is the configured value.
        assert_eq!(next_rung(2, CycleVerdict::DhanFailed, 3), 3);
        assert_eq!(next_rung(3, CycleVerdict::DhanFailed, 3), 3);
        assert_eq!(next_rung(0, CycleVerdict::FloorExhausted, 3), 3);
        // failure_arms_ladder is total over the 7-variant error enum —
        // exactly 3 arm, 4 do not (a new variant must pick a side here).
        let arms = [
            failure_arms_ladder(&CadenceFetchError::RateLimited {
                retry_after_ms: None,
            }),
            failure_arms_ladder(&CadenceFetchError::Timeout),
            failure_arms_ladder(&CadenceFetchError::Transport),
            failure_arms_ladder(&CadenceFetchError::Empty),
            failure_arms_ladder(&CadenceFetchError::QueueDelay),
            failure_arms_ladder(&CadenceFetchError::Auth),
            failure_arms_ladder(&CadenceFetchError::Malformed),
        ];
        assert_eq!(arms.iter().filter(|a| **a).count(), 3);
        assert_eq!(arms, [true, true, true, false, false, false, false]);
    }

    #[test]
    fn test_ladder_queue_delay_is_non_arming_but_retryable() {
        // F1(iii), 2026-07-15: a shared-limiter queue deadline miss is
        // SELF-INFLICTED pacing — it must NEVER walk the anchor ladder…
        assert!(!failure_arms_ladder(&CadenceFetchError::QueueDelay));
        // …but it IS retryable in-cycle (the pacing spill clears within
        // the window; one more gated attempt is legitimate).
        assert!(may_retry_in_cycle(
            &CadenceFetchError::QueueDelay,
            0,
            1,
            5_000,
            1_500,
            15_000
        ));
    }

    #[test]
    fn test_ladder_any_failure_arming_amplitude_1_oscillation() {
        // F7, 2026-07-15 — CURRENT CONTRACT PIN (Assumed, flagged): the
        // cycle verdict arms on ANY arming failure even when an in-cycle
        // retry recovered the leg, so a perfectly alternating fail/clean
        // minute pattern oscillates the anchor rung 0 ↔ 1 forever —
        // amplitude 1, never runaway. This test DOCUMENTS the accepted
        // oscillation; a future "recovered-in-cycle does not arm"
        // refinement (operator decision) would change these assertions.
        let max = 5;
        let mut rung = 0u8;
        for _ in 0..4 {
            rung = next_rung(rung, CycleVerdict::DhanFailed, max);
            assert_eq!(rung, 1, "failed minute shifts home → rung 1");
            rung = next_rung(rung, CycleVerdict::Clean, max);
            assert_eq!(rung, 0, "clean minute recovers rung 1 → home");
        }
        // The oscillation is bounded at amplitude 1: it can never walk
        // deeper without CONSECUTIVE failing cycles.
        assert!(rung <= 1);
    }

    #[test]
    fn test_spot_concurrency_ladder_degrades_after_2_dirty_recovers_after_3_clean() {
        // The operator's exact rules (defaults 2/3, Assumed): degrade ONE
        // step after 2 CONSECUTIVE dirty cycles; recover one step after 3
        // consecutive fully-clean cycles at the current step.
        let mut l = StreakLadder::starting_at(0);
        // One-off 429 never degrades…
        assert_eq!(l.advance(true, 2, 3, 0, SPOT_CONCURRENCY_MAX_STEP), None);
        assert_eq!(l.step, 0);
        // …a clean cycle resets the dirty streak…
        assert_eq!(l.advance(false, 2, 3, 0, SPOT_CONCURRENCY_MAX_STEP), None);
        assert_eq!(l.advance(true, 2, 3, 0, SPOT_CONCURRENCY_MAX_STEP), None);
        assert_eq!(l.step, 0, "dirty-clean-dirty is never 'continuous'");
        // …2 CONSECUTIVE dirty cycles degrade exactly one step.
        assert_eq!(
            l.advance(true, 2, 3, 0, SPOT_CONCURRENCY_MAX_STEP),
            Some(StreakShift::Degraded)
        );
        assert_eq!(l.step, 1);
        // Walk the full ladder 1 → 2 → 3 (each needing a fresh 2-streak).
        for expected in [2u8, 3] {
            assert_eq!(l.advance(true, 2, 3, 0, SPOT_CONCURRENCY_MAX_STEP), None);
            assert_eq!(
                l.advance(true, 2, 3, 0, SPOT_CONCURRENCY_MAX_STEP),
                Some(StreakShift::Degraded)
            );
            assert_eq!(l.step, expected);
        }
        // Dirty AT the max step holds the step (never past sequential).
        assert_eq!(l.advance(true, 2, 3, 0, SPOT_CONCURRENCY_MAX_STEP), None);
        assert_eq!(l.advance(true, 2, 3, 0, SPOT_CONCURRENCY_MAX_STEP), None);
        assert_eq!(l.step, 3);
        // Recovery: 2 clean cycles are NOT enough; a dirty cycle resets
        // the clean streak; 3 CONSECUTIVE clean cycles climb one step.
        assert_eq!(l.advance(false, 2, 3, 0, SPOT_CONCURRENCY_MAX_STEP), None);
        assert_eq!(l.advance(false, 2, 3, 0, SPOT_CONCURRENCY_MAX_STEP), None);
        assert_eq!(l.advance(true, 2, 3, 0, SPOT_CONCURRENCY_MAX_STEP), None);
        assert_eq!(l.step, 3, "a dirty cycle resets the clean streak");
        for expected in [2u8, 1, 0] {
            assert_eq!(l.advance(false, 2, 3, 0, SPOT_CONCURRENCY_MAX_STEP), None);
            assert_eq!(l.advance(false, 2, 3, 0, SPOT_CONCURRENCY_MAX_STEP), None);
            assert_eq!(
                l.advance(false, 2, 3, 0, SPOT_CONCURRENCY_MAX_STEP),
                Some(StreakShift::Recovered)
            );
            assert_eq!(l.step, expected);
        }
        // Clean at step 0 stays home (never climbs past the floor).
        for _ in 0..5 {
            assert_eq!(l.advance(false, 2, 3, 0, SPOT_CONCURRENCY_MAX_STEP), None);
        }
        assert_eq!(l.step, 0);
    }

    #[test]
    fn test_groww_shape_ladder_all_choice_transitions() {
        // The coordinator-required tier-transition matrix (2026-07-15):
        // choice 1→2, 2→3, 3→2, 2→1 — steps 0→1, 1→2, 2→1, 1→0 — under
        // the shared 2-dirty / 3-clean rules.
        let mut l = StreakLadder::starting_at(0);
        // 1 → 2.
        assert_eq!(l.advance(true, 2, 3, 0, GROWW_SHAPE_MAX_STEP), None);
        assert_eq!(
            l.advance(true, 2, 3, 0, GROWW_SHAPE_MAX_STEP),
            Some(StreakShift::Degraded)
        );
        assert_eq!(l.step, 1);
        // 2 → 3 (the LAST resort — the ladder tops out here).
        assert_eq!(l.advance(true, 2, 3, 0, GROWW_SHAPE_MAX_STEP), None);
        assert_eq!(
            l.advance(true, 2, 3, 0, GROWW_SHAPE_MAX_STEP),
            Some(StreakShift::Degraded)
        );
        assert_eq!(l.step, 2);
        assert_eq!(l.advance(true, 2, 3, 0, GROWW_SHAPE_MAX_STEP), None);
        assert_eq!(l.advance(true, 2, 3, 0, GROWW_SHAPE_MAX_STEP), None);
        assert_eq!(l.step, GROWW_SHAPE_MAX_STEP, "never past choice 3");
        // 3 → 2.
        assert_eq!(l.advance(false, 2, 3, 0, GROWW_SHAPE_MAX_STEP), None);
        assert_eq!(l.advance(false, 2, 3, 0, GROWW_SHAPE_MAX_STEP), None);
        assert_eq!(
            l.advance(false, 2, 3, 0, GROWW_SHAPE_MAX_STEP),
            Some(StreakShift::Recovered)
        );
        assert_eq!(l.step, 1);
        // 2 → 1 (back to the all-7 burst; never stuck from a one-off).
        assert_eq!(l.advance(false, 2, 3, 0, GROWW_SHAPE_MAX_STEP), None);
        assert_eq!(l.advance(false, 2, 3, 0, GROWW_SHAPE_MAX_STEP), None);
        assert_eq!(
            l.advance(false, 2, 3, 0, GROWW_SHAPE_MAX_STEP),
            Some(StreakShift::Recovered)
        );
        assert_eq!(l.step, 0);
    }

    #[test]
    fn test_spot_group_index_encodes_operator_groupings() {
        // Step 0: [[4]] — all 4 at the anchor slot.
        for k in 0..4 {
            assert_eq!(spot_group_index(0, k), 0);
        }
        // Step 1: [[3],[1]] — VIX (idx 3, the advisory) alone second.
        assert_eq!(spot_group_index(1, 0), 0);
        assert_eq!(spot_group_index(1, 1), 0);
        assert_eq!(spot_group_index(1, 2), 0);
        assert_eq!(spot_group_index(1, 3), 1);
        // Step 2: [[2],[2]].
        assert_eq!(spot_group_index(2, 0), 0);
        assert_eq!(spot_group_index(2, 1), 0);
        assert_eq!(spot_group_index(2, 2), 1);
        assert_eq!(spot_group_index(2, 3), 1);
        // Step 3 (and any clamp past it): fully sequential.
        for k in 0..4 {
            assert_eq!(spot_group_index(3, k), k);
            assert_eq!(spot_group_index(9, k), k);
        }
        // The structural floor per window cap: no group may exceed the cap.
        assert_eq!(min_spot_step_for_cap(5), 0);
        assert_eq!(min_spot_step_for_cap(4), 0);
        assert_eq!(min_spot_step_for_cap(3), 1);
        assert_eq!(min_spot_step_for_cap(2), 2);
        assert_eq!(min_spot_step_for_cap(1), 3);
        assert_eq!(min_spot_step_for_cap(0), 3, "fail-closed sequential");
        for cap in 1..=5u32 {
            let floor = min_spot_step_for_cap(cap);
            for step in floor..=SPOT_CONCURRENCY_MAX_STEP {
                for group in 0..4 {
                    let size = (0..4)
                        .filter(|k| spot_group_index(step, *k) == group)
                        .count();
                    assert!(
                        size as u32 <= cap,
                        "cap {cap} step {step} group {group} size {size}"
                    );
                }
            }
        }
        // A floored ladder starts AND stays at/above its floor.
        let mut l = StreakLadder::starting_at(min_spot_step_for_cap(2));
        assert_eq!(l.step, 2);
        for _ in 0..10 {
            l.advance(
                false,
                2,
                3,
                min_spot_step_for_cap(2),
                SPOT_CONCURRENCY_MAX_STEP,
            );
        }
        assert_eq!(l.step, 2, "recovery never climbs above the cap floor");
    }

    #[test]
    fn test_groww_wave_indices_encode_three_choice_shapes() {
        // Choice 1 (step 0): :00 all-7-parallel.
        assert_eq!(groww_wave_indices(0), (0, 0, 0));
        // Choice 2 (step 1): :01 chains, :02 ALL 4 spots (VIX included).
        assert_eq!(groww_wave_indices(1), (1, 2, 2));
        // Choice 3 (step 2, last resort): :01 chains, :02 core spots,
        // :03 VIX alone (deliberately lowest priority).
        assert_eq!(groww_wave_indices(2), (1, 2, 3));
        // Clamp past the max = choice 3.
        assert_eq!(groww_wave_indices(7), (1, 2, 3));
    }
}
