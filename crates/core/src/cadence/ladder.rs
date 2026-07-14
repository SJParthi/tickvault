//! The Dhan failure ladder (design §3) + the in-cycle retry policy.
//!
//! The ladder shifts the NEXT cycle's Dhan anchor 1s earlier per failing
//! cycle (:55 → :54 → … → :50 floor at rung 5 — you cannot rewind time
//! inside a cycle) and steps back ONE rung per fully-clean cycle
//! (hysteresis — a reset-to-:55 would oscillate full-amplitude against a
//! flapping vendor; unanimous designer choice, Assumed pending operator
//! confirm). The rung resets to 0 at day start / process boot (the ladder
//! is a day-scoped in-memory variable, NOT a cycle FSM state).

use super::executor::CadenceFetchError;

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
        // failure_arms_ladder is total over the 6-variant error enum —
        // exactly 3 arm, 3 do not (a new variant must pick a side here).
        let arms = [
            failure_arms_ladder(&CadenceFetchError::RateLimited {
                retry_after_ms: None,
            }),
            failure_arms_ladder(&CadenceFetchError::Timeout),
            failure_arms_ladder(&CadenceFetchError::Transport),
            failure_arms_ladder(&CadenceFetchError::Empty),
            failure_arms_ladder(&CadenceFetchError::Auth),
            failure_arms_ladder(&CadenceFetchError::Malformed),
        ];
        assert_eq!(arms.iter().filter(|a| **a).count(), 3);
        assert_eq!(arms, [true, true, true, false, false, false]);
    }
}
