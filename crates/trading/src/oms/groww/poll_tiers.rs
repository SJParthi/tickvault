//! Pure poll-tier scheduler for tracked Groww orders (ORD-PR-2 scope delta,
//! build-lead-approved 2026-07-15) — the adaptive 2s/5s/15s cadence of
//! design §4.8, as PURE tier assignment + worst-case budget arithmetic.
//! No clock, no I/O, no channel — the executor (ORD-PR-3) drives it.
//!
//! # The tier model
//! - **HOT (2s):** within the 30s post-mutation window; capped at 3 orders.
//! - **WARM (5s):** until first fill or terminal; capped at 2 orders.
//! - **STEADY (15s):** everything else, INCLUDING every cap-overflow order —
//!   tier demotion STRETCHES the interval; it NEVER drops a poll.
//! - Tracked-open cap: 8 orders (design §4.8 / C's cap).
//! - The reserved 1/s ambiguity-resolution slice is OCCUPANCY-INVARIANT —
//!   modeled as a constant 60 reads/min term, never schedulable/stealable.
//!
//! # The budget ratchet
//! Full 8-order saturation: 3×30 (hot) + 2×12 (warm) + 3×4 (steady)
//! + 60 (reserved resolution) + 10 (post-mutation confirms at the mutation
//! cap) + 2 (reconcile sweeps) = **198 ≤ 200/min reads self-cap** — pinned
//! by a build-failing test below (worst-case-INCLUSIVE arithmetic, hostile
//! F-8).

// --- Poll-tier constants. The 9 reads-family / cap / cadence values now live
// --- in tickvault-common (landed in PR-A0); imported here under the local
// --- identifier names so the budget ratchet + tests read unchanged. The two
// --- reads-family caps (READS_PER_MINUTE_CAP, RESERVED_RESOLUTION_READS_PER_MIN)
// --- are u32 in common, so they carry an `as u64` at each u64 arithmetic site
// --- below — common stays authoritative; the value is never widened there.

/// Delay of the one-shot post-mutation confirm poll (graft 4). Consumed by the
/// ORD-PR-3 executor; only the pinning test references it today, so allow the
/// otherwise-unused re-export in a feature build with tests excluded.
#[allow(unused_imports)]
pub(crate) use tickvault_common::constants::GROWW_ORDER_POST_MUTATION_CONFIRM_DELAY_MS as POST_MUTATION_CONFIRM_DELAY_MS;
pub(crate) use tickvault_common::constants::{
    GROWW_ORDER_MAX_HOT_TIER_ORDERS as MAX_HOT, GROWW_ORDER_MAX_TRACKED_OPEN_ORDERS as MAX_TRACKED,
    GROWW_ORDER_MAX_WARM_TIER_ORDERS as MAX_WARM,
    GROWW_ORDER_READS_PER_MINUTE_CAP as READS_PER_MINUTE_CAP,
    GROWW_ORDER_RESERVED_RESOLUTION_READS_PER_MIN as RESERVED_RESOLUTION_READS_PER_MIN,
    GROWW_ORDER_STATUS_POLL_HOT_SECS as HOT_SECS,
    GROWW_ORDER_STATUS_POLL_STEADY_SECS as STEADY_SECS,
    GROWW_ORDER_STATUS_POLL_WARM_SECS as WARM_SECS,
};

/// HOT-tier residency window after a mutation (seconds). Genuinely module-local.
#[allow(dead_code)] // consumed by the ORD-PR-3 executor (hot-window residency)
pub(crate) const HOT_WINDOW_SECS: u64 = 30;
/// Post-mutation confirm polls budget line (at the 10-mutations/min-class
/// worst case — one +500ms confirm each). Genuinely module-local.
pub(crate) const POST_MUTATION_CONFIRMS_PER_MIN: u64 = 10;
/// Reconcile order-list sweep reads per minute (60s cadence + margin).
/// Genuinely module-local.
pub(crate) const RECONCILE_SWEEP_READS_PER_MIN: u64 = 2;

/// A poll cadence tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PollTier {
    /// 2s — inside the post-mutation hot window.
    Hot,
    /// 5s — awaiting first fill / terminal.
    Warm,
    /// 15s — everything else (including all cap overflow).
    Steady,
}

impl PollTier {
    /// The poll interval of this tier, in seconds.
    #[must_use]
    pub fn interval_secs(self) -> u64 {
        match self {
            Self::Hot => HOT_SECS,
            Self::Warm => WARM_SECS,
            Self::Steady => STEADY_SECS,
        }
    }

    /// Polls per minute at this tier's cadence.
    #[must_use]
    pub fn polls_per_minute(self) -> u64 {
        60 / self.interval_secs()
    }
}

/// Per-order tier eligibility — pure inputs the executor derives from its
/// tracked state (no clock here: the caller resolves "within the 30s
/// post-mutation window" against ITS clock).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TierEligibility {
    /// A mutation on this order happened within [`HOT_WINDOW_SECS`].
    pub hot_eligible: bool,
    /// No first fill yet and not terminal (the warm criterion).
    pub warm_eligible: bool,
}

/// Tier-assignment refusal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PollTierError {
    /// More orders than [`MAX_TRACKED`] — the executor must not track them
    /// (the cap is enforced at intent admission, not here; this is the
    /// fail-closed backstop).
    TooManyTracked {
        /// Offered order count.
        offered: usize,
    },
}

/// Assign a tier to EVERY offered order — pure, total over `len ≤ 8`,
/// deterministic in input order (the caller pre-sorts by priority, e.g.
/// most-recent mutation first).
///
/// Cap semantics: hot-eligible orders take HOT slots until [`MAX_HOT`];
/// overflow hot-eligibles are DEMOTED (stretched) — they compete for WARM
/// slots if warm-eligible, else land STEADY. Warm-eligibles take WARM slots
/// until [`MAX_WARM`]; overflow lands STEADY. **No order is ever dropped**
/// — the output length always equals the input length.
///
/// ⚠ Per-order intervals are SLOT-BOUND but NON-MONOTONE under caller
/// re-sort: a re-prioritized input order can move an order between tiers
/// across calls (e.g. HOT → STEADY when it loses its slot), so an
/// individual order's observed cadence may lengthen and shorten across
/// scheduling rounds. The BUDGET invariant (≤ the reads self-cap) holds for
/// every split regardless.
pub fn assign_poll_tiers(orders: &[TierEligibility]) -> Result<Vec<PollTier>, PollTierError> {
    if orders.len() > MAX_TRACKED {
        return Err(PollTierError::TooManyTracked {
            offered: orders.len(),
        });
    }
    let mut hot_used = 0_usize;
    let mut warm_used = 0_usize;
    let mut out = Vec::with_capacity(orders.len());
    for o in orders {
        let tier = if o.hot_eligible && hot_used < MAX_HOT {
            hot_used += 1;
            PollTier::Hot
        } else if (o.warm_eligible || o.hot_eligible) && warm_used < MAX_WARM {
            // A hot-overflow order stretches into WARM before STEADY —
            // demotion stretches, never drops.
            warm_used += 1;
            PollTier::Warm
        } else {
            PollTier::Steady
        };
        out.push(tier);
    }
    Ok(out)
}

/// Reads/minute implied by a concrete tier assignment, INCLUDING the
/// occupancy-invariant constant terms (reserved resolution slice +
/// post-mutation confirms at the mutation cap + reconcile sweeps) — the
/// worst-case-inclusive accounting of hostile F-8.
#[must_use]
pub fn reads_per_minute(assignments: &[PollTier]) -> u64 {
    let polling: u64 = assignments.iter().map(|t| t.polls_per_minute()).sum();
    polling
        + RESERVED_RESOLUTION_READS_PER_MIN as u64
        + POST_MUTATION_CONFIRMS_PER_MIN
        + RECONCILE_SWEEP_READS_PER_MIN
}

/// The worst-case reads/minute under FULL saturation: [`MAX_TRACKED`] orders
/// with maximal tier occupancy (3 HOT + 2 WARM + 3 STEADY) plus every
/// constant term. Pure arithmetic — the budget the no-rest §10 grant table
/// carries.
#[must_use]
pub fn worst_case_reads_per_minute() -> u64 {
    let hot = (MAX_HOT as u64) * (60 / HOT_SECS);
    let warm = (MAX_WARM as u64) * (60 / WARM_SECS);
    let steady = ((MAX_TRACKED - MAX_HOT - MAX_WARM) as u64) * (60 / STEADY_SECS);
    hot + warm
        + steady
        + RESERVED_RESOLUTION_READS_PER_MIN as u64
        + POST_MUTATION_CONFIRMS_PER_MIN
        + RECONCILE_SWEEP_READS_PER_MIN
}

// Compile-time ratchet: the worst case can never silently exceed the
// self-cap (a constants change that breaks the arithmetic fails the build
// here, before any test runs).
const _: () = assert!(
    (MAX_HOT as u64) * (60 / HOT_SECS)
        + (MAX_WARM as u64) * (60 / WARM_SECS)
        + ((MAX_TRACKED - MAX_HOT - MAX_WARM) as u64) * (60 / STEADY_SECS)
        + RESERVED_RESOLUTION_READS_PER_MIN as u64
        + POST_MUTATION_CONFIRMS_PER_MIN
        + RECONCILE_SWEEP_READS_PER_MIN
        <= READS_PER_MINUTE_CAP as u64
);

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const HOT_ORDER: TierEligibility = TierEligibility {
        hot_eligible: true,
        warm_eligible: true,
    };
    const WARM_ORDER: TierEligibility = TierEligibility {
        hot_eligible: false,
        warm_eligible: true,
    };
    const STEADY_ORDER: TierEligibility = TierEligibility {
        hot_eligible: false,
        warm_eligible: false,
    };

    // --- THE ratchet: full 8-order saturation arithmetic ---

    #[test]
    fn test_ratchet_max_occupancy_reads_per_minute_is_198_and_under_cap() {
        // 3×30 + 2×12 + 3×4 + 60 + 10 + 2 = 198.
        assert_eq!(worst_case_reads_per_minute(), 198);
        assert!(worst_case_reads_per_minute() <= READS_PER_MINUTE_CAP as u64);

        // And the concrete saturated assignment computes the same number.
        let orders = [
            HOT_ORDER,
            HOT_ORDER,
            HOT_ORDER,
            WARM_ORDER,
            WARM_ORDER,
            STEADY_ORDER,
            STEADY_ORDER,
            STEADY_ORDER,
        ];
        let tiers = assign_poll_tiers(&orders).unwrap();
        assert_eq!(
            tiers,
            vec![
                PollTier::Hot,
                PollTier::Hot,
                PollTier::Hot,
                PollTier::Warm,
                PollTier::Warm,
                PollTier::Steady,
                PollTier::Steady,
                PollTier::Steady,
            ]
        );
        assert_eq!(reads_per_minute(&tiers), 198);
    }

    #[test]
    fn test_constants_pinned() {
        assert_eq!(HOT_SECS, 2);
        assert_eq!(WARM_SECS, 5);
        assert_eq!(STEADY_SECS, 15);
        assert_eq!(MAX_HOT, 3);
        assert_eq!(MAX_WARM, 2);
        assert_eq!(MAX_TRACKED, 8);
        assert_eq!(READS_PER_MINUTE_CAP, 200);
        assert_eq!(RESERVED_RESOLUTION_READS_PER_MIN, 60);
        assert_eq!(POST_MUTATION_CONFIRM_DELAY_MS, 500);
        assert_eq!(HOT_WINDOW_SECS, 30);
        assert_eq!(PollTier::Hot.polls_per_minute(), 30);
        assert_eq!(PollTier::Warm.polls_per_minute(), 12);
        assert_eq!(PollTier::Steady.polls_per_minute(), 4);
    }

    // --- demotion stretches, never drops ---

    #[test]
    fn test_hot_overflow_demotes_to_warm_then_steady_never_drops() {
        // 6 hot-eligible orders: 3 HOT, 2 stretched to WARM, 1 to STEADY.
        let orders = [HOT_ORDER; 6];
        let tiers = assign_poll_tiers(&orders).unwrap();
        assert_eq!(tiers.len(), 6, "no poll dropped");
        assert_eq!(tiers.iter().filter(|t| **t == PollTier::Hot).count(), 3);
        assert_eq!(tiers.iter().filter(|t| **t == PollTier::Warm).count(), 2);
        assert_eq!(tiers.iter().filter(|t| **t == PollTier::Steady).count(), 1);
    }

    #[test]
    fn test_warm_overflow_lands_steady() {
        let orders = [WARM_ORDER; 5];
        let tiers = assign_poll_tiers(&orders).unwrap();
        assert_eq!(tiers.iter().filter(|t| **t == PollTier::Warm).count(), 2);
        assert_eq!(tiers.iter().filter(|t| **t == PollTier::Steady).count(), 3);
    }

    #[test]
    fn test_empty_and_cap_boundaries() {
        assert_eq!(assign_poll_tiers(&[]).unwrap(), Vec::<PollTier>::new());
        // Exactly at the tracked cap is fine.
        assert!(assign_poll_tiers(&[STEADY_ORDER; MAX_TRACKED]).is_ok());
        // Beyond the cap is refused (fail-closed backstop).
        assert_eq!(
            assign_poll_tiers(&[STEADY_ORDER; MAX_TRACKED + 1]),
            Err(PollTierError::TooManyTracked {
                offered: MAX_TRACKED + 1
            })
        );
    }

    #[test]
    fn test_reserved_slice_is_occupancy_invariant() {
        // Zero tracked orders still budget the full reserved slice +
        // constant terms — the slice is never schedulable.
        let zero = reads_per_minute(&[]);
        assert_eq!(
            zero,
            RESERVED_RESOLUTION_READS_PER_MIN as u64
                + POST_MUTATION_CONFIRMS_PER_MIN
                + RECONCILE_SWEEP_READS_PER_MIN
        );
    }

    // --- property: EVERY occupancy split stays ≤ the self-cap ---

    proptest! {
        #[test]
        fn prop_every_occupancy_split_under_reads_cap(
            flags in proptest::collection::vec((proptest::bool::ANY, proptest::bool::ANY), 0..=MAX_TRACKED),
        ) {
            let orders: Vec<TierEligibility> = flags
                .into_iter()
                .map(|(h, w)| TierEligibility { hot_eligible: h, warm_eligible: w })
                .collect();
            let tiers = assign_poll_tiers(&orders).unwrap();
            prop_assert_eq!(tiers.len(), orders.len(), "no poll dropped");
            prop_assert!(reads_per_minute(&tiers) <= READS_PER_MINUTE_CAP as u64);
            // Caps hold in every split.
            prop_assert!(tiers.iter().filter(|t| **t == PollTier::Hot).count() <= MAX_HOT);
            prop_assert!(tiers.iter().filter(|t| **t == PollTier::Warm).count() <= MAX_WARM);
        }
    }
}
