//! Fail-closed self-capping dual-tier rate budget for the Groww order lane
//! (design §4.8; ORD-PR-3).
//!
//! Two request FAMILIES, each with a per-second AND a per-minute ceiling
//! (Dhan's [`super::super::rate_limiter`] is single-tier — this is a NEW
//! dual-tier type):
//!
//! | Family | Doc ceiling (`15` §rate-limits) | Self-cap (this module) |
//! |--------|---------------------------------|------------------------|
//! | Orders (place/modify/cancel) | 10/s · 250/min | **5/s · 100/min** |
//! | Reads (status/list/detail/trades) | 20/s · 500/min | **8/s · 200/min** |
//!
//! The self-caps are ≤ 50 % / ≤ 40 % of the documented family limits — honest
//! headroom for BruteX co-tenancy (U-5) + the §38 1m REST legs, since the
//! bucket SCOPE (per key / token / account) is Unknown (U-5/O-6). Every
//! self-cap is `const`-asserted ≤ the documented ceiling so a future tune-up
//! can only ever tighten, never breach, the doc — a breach fails the build.
//!
//! Reads carry a RESERVED resolution slice (1/s · 60/min): ambiguity
//! resolution polls draw the reserved slice FIRST, so routine per-order
//! polling can never starve the resolution ladder (design §4.8; the honest
//! caveat is that the reserved slice is a SELF-CAP-side reservation only — it
//! grants no immunity from a broker-side 429, hostile P19).
//!
//! The poll-tier scheduler ([`super::poll_tiers`]) is the read-side occupancy
//! model; [`worst_case_reads_per_minute`] (re-exported from it) is the number
//! the no-rest §10 grant table carries, and a `const _` assert here ties the
//! two together — the scheduler's worst case can never exceed this module's
//! per-minute reads cap.
//!
//! Cold path (order flow is human-scale). GCRA via the `governor` crate — the
//! same engine as the Dhan limiter.

use std::num::NonZeroU32;

use governor::{
    Quota, RateLimiter,
    clock::DefaultClock,
    state::{InMemoryState, NotKeyed},
};

use super::poll_tiers;

// ---------------------------------------------------------------------------
// Self-caps + documented ceilings (module-local; move to tickvault-common in
// ORD-PR-1). Every value is `const`-asserted ≤ its doc ceiling below.
// ---------------------------------------------------------------------------

/// Orders family per-second self-cap (single-sourced from tickvault-common).
pub(crate) const ORDERS_PER_SECOND_CAP: u32 =
    tickvault_common::constants::GROWW_ORDER_MUTATIONS_PER_SECOND_CAP;
/// Orders family per-minute self-cap (single-sourced from tickvault-common).
pub(crate) const ORDERS_PER_MINUTE_CAP: u32 =
    tickvault_common::constants::GROWW_ORDER_MUTATIONS_PER_MINUTE_CAP;
/// Reads family per-second self-cap (single-sourced from tickvault-common).
pub(crate) const READS_PER_SECOND_CAP: u32 =
    tickvault_common::constants::GROWW_ORDER_READS_PER_SECOND_CAP;
/// Reads family per-minute self-cap (single-sourced from tickvault-common).
pub(crate) const READS_PER_MINUTE_CAP: u32 =
    tickvault_common::constants::GROWW_ORDER_READS_PER_MINUTE_CAP;
/// Reserved resolution read slice — 1/s, i.e. 60/min (design §4.8). KEPT LOCAL:
/// the common `GROWW_ORDER_RESERVED_RESOLUTION_READS_PER_MIN` is 60/min — a UNIT
/// mismatch (per-minute vs this per-second slice), so it is not single-sourced.
// moves to tickvault-common GROWW_ORDER_* in ORD-PR-1
pub(crate) const RESERVED_RESOLUTION_PER_SECOND: u32 = 1;

/// Documented Orders-family ceilings (`15` §rate-limits — TYPE-LEVEL POOLED).
// moves to tickvault-common GROWW_ORDER_* in ORD-PR-1
pub(crate) const DOC_ORDERS_PER_SECOND: u32 = 10;
/// Documented Orders-family per-minute ceiling.
// moves to tickvault-common GROWW_ORDER_* in ORD-PR-1
pub(crate) const DOC_ORDERS_PER_MINUTE: u32 = 250;
/// Documented Non-Trading (reads) per-second ceiling.
// moves to tickvault-common GROWW_ORDER_* in ORD-PR-1
pub(crate) const DOC_READS_PER_SECOND: u32 = 20;
/// Documented Non-Trading (reads) per-minute ceiling.
// moves to tickvault-common GROWW_ORDER_* in ORD-PR-1
pub(crate) const DOC_READS_PER_MINUTE: u32 = 500;

// Compile-time fail-closed guards: every self-cap ≤ the documented ceiling.
const _: () = assert!(ORDERS_PER_SECOND_CAP <= DOC_ORDERS_PER_SECOND);
const _: () = assert!(ORDERS_PER_MINUTE_CAP <= DOC_ORDERS_PER_MINUTE);
const _: () = assert!(READS_PER_SECOND_CAP <= DOC_READS_PER_SECOND);
const _: () = assert!(READS_PER_MINUTE_CAP <= DOC_READS_PER_MINUTE);
// The reserved resolution slice is carved OUT of the reads-per-second cap.
const _: () = assert!(RESERVED_RESOLUTION_PER_SECOND <= READS_PER_SECOND_CAP);
// The scheduler's worst-case reads/min can never exceed the reads per-minute
// self-cap (poll_tiers carries its own equivalent assert; this restates it in
// the budget's own units to bind the two modules). `poll_tiers`'
// `worst_case_reads_per_minute()` is not `const`, so recompute it here from the
// same `pub(crate)` scheduler constants — a runtime test cross-checks the two.
const fn worst_case_reads_const() -> u64 {
    (poll_tiers::MAX_HOT as u64) * (60 / poll_tiers::HOT_SECS)
        + (poll_tiers::MAX_WARM as u64) * (60 / poll_tiers::WARM_SECS)
        + ((poll_tiers::MAX_TRACKED - poll_tiers::MAX_HOT - poll_tiers::MAX_WARM) as u64)
            * (60 / poll_tiers::STEADY_SECS)
        + poll_tiers::RESERVED_RESOLUTION_READS_PER_MIN as u64
        + poll_tiers::POST_MUTATION_CONFIRMS_PER_MIN
        + poll_tiers::RECONCILE_SWEEP_READS_PER_MIN
}
const _: () = assert!(worst_case_reads_const() <= READS_PER_MINUTE_CAP as u64);

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// The two rate FAMILIES the Groww order surface uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RateFamily {
    /// place / modify / cancel — the Orders family.
    Orders,
    /// status / list / detail / trades — the Non-Trading (reads) family.
    Reads,
}

impl RateFamily {
    /// Stable metric-label for `tv_groww_order_rate_limited_total{family}`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Orders => "orders",
            Self::Reads => "reads",
        }
    }
}

/// The outcome of a budget check — a pure verdict; the caller does the wait /
/// the counter increment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetDecision {
    /// A slot was consumed — the request may proceed.
    Allowed,
    /// No slot available now — the caller must wait (routine) or the
    /// resolution ladder pauses (resolution).
    Denied,
}

impl BudgetDecision {
    /// Whether the request may proceed.
    #[must_use]
    pub const fn is_allowed(self) -> bool {
        matches!(self, Self::Allowed)
    }
}

/// Which read PRIORITY drew the slot — resolution outranks routine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadSlot {
    /// The reserved resolution slice granted the slot.
    Reserved,
    /// The shared reads budget granted the slot.
    Shared,
    /// No slot available.
    Denied,
}

type DirectLimiter = RateLimiter<NotKeyed, InMemoryState, DefaultClock>;

fn per_second(n: u32) -> DirectLimiter {
    // n is a compile-time cap ≥ 1 (asserted above); the fallback keeps this
    // panic-free without an unwrap/expect on the hot construction path.
    let burst = NonZeroU32::new(n).unwrap_or(NonZeroU32::MIN);
    RateLimiter::direct(Quota::per_second(burst))
}

fn per_minute(n: u32) -> DirectLimiter {
    let burst = NonZeroU32::new(n).unwrap_or(NonZeroU32::MIN);
    RateLimiter::direct(Quota::per_minute(burst))
}

/// The dual-tier, dual-family Groww order rate budget.
///
/// Fail-closed: a mutation requires BOTH Orders-family tiers to grant; a read
/// requires the reads-family tiers (resolution reads try the reserved slice
/// first). A denial NEVER proceeds — the caller waits or defers.
pub struct GrowwRateBudget {
    orders_sec: DirectLimiter,
    orders_min: DirectLimiter,
    reads_sec: DirectLimiter,
    reads_min: DirectLimiter,
    /// The reserved resolution slice (1/s) — resolution reads draw it first.
    reserved_sec: DirectLimiter,
}

impl Default for GrowwRateBudget {
    fn default() -> Self {
        Self::new()
    }
}

impl GrowwRateBudget {
    /// Construct the budget at the module self-caps.
    #[must_use]
    pub fn new() -> Self {
        Self {
            orders_sec: per_second(ORDERS_PER_SECOND_CAP),
            orders_min: per_minute(ORDERS_PER_MINUTE_CAP),
            reads_sec: per_second(READS_PER_SECOND_CAP),
            reads_min: per_minute(READS_PER_MINUTE_CAP),
            reserved_sec: per_second(RESERVED_RESOLUTION_PER_SECOND),
        }
    }

    /// Check whether a MUTATION (place/modify/cancel) may proceed — BOTH
    /// Orders-family tiers must grant.
    ///
    /// Ordering: the per-second tier (fast-recovering) is checked first, so a
    /// per-minute denial after a per-second grant wastes at most one
    /// per-second cell (recovers in ≤1 s) — always the fail-closed direction.
    #[must_use]
    pub fn check_mutation(&self) -> BudgetDecision {
        if self.orders_sec.check().is_ok() && self.orders_min.check().is_ok() {
            BudgetDecision::Allowed
        } else {
            BudgetDecision::Denied
        }
    }

    /// Check a ROUTINE read (per-order polling / reconcile sweep) — the shared
    /// reads budget only; the reserved slice is never touched.
    #[must_use]
    pub fn check_routine_read(&self) -> BudgetDecision {
        if self.reads_sec.check().is_ok() && self.reads_min.check().is_ok() {
            BudgetDecision::Allowed
        } else {
            BudgetDecision::Denied
        }
    }

    /// Check a RESOLUTION read (ambiguity ladder / post-mutation confirm) —
    /// draws the reserved slice FIRST, falling back to the shared reads budget
    /// (still counted against the reads tiers). Resolution therefore outranks
    /// routine polling: even a saturated shared budget leaves ≤1/s for the
    /// ladder.
    #[must_use]
    pub fn check_resolution_read(&self) -> ReadSlot {
        if self.reserved_sec.check().is_ok() {
            // The reserved grant stands independently of the shared budget —
            // that is the whole point of the reservation (resolution outranks
            // routine polling even when the shared reads cells are exhausted).
            return ReadSlot::Reserved;
        }
        if self.reads_sec.check().is_ok() && self.reads_min.check().is_ok() {
            ReadSlot::Shared
        } else {
            ReadSlot::Denied
        }
    }
}

// ---------------------------------------------------------------------------
// Pure descriptor (the numbers the no-rest §10 grant table carries)
// ---------------------------------------------------------------------------

/// The budget's self-caps + doc ceilings + worst-case reads, as pure data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BudgetDescriptor {
    /// Orders per-second self-cap.
    pub orders_per_second: u32,
    /// Orders per-minute self-cap.
    pub orders_per_minute: u32,
    /// Reads per-second self-cap.
    pub reads_per_second: u32,
    /// Reads per-minute self-cap.
    pub reads_per_minute: u32,
    /// Reserved resolution reads per second.
    pub reserved_resolution_per_second: u32,
    /// The scheduler's worst-case reads/minute under full 8-order occupancy.
    pub worst_case_reads_per_minute: u64,
}

/// The static budget descriptor (pure — the grant-table source of truth).
#[must_use]
pub const fn describe_budget() -> BudgetDescriptor {
    BudgetDescriptor {
        orders_per_second: ORDERS_PER_SECOND_CAP,
        orders_per_minute: ORDERS_PER_MINUTE_CAP,
        reads_per_second: READS_PER_SECOND_CAP,
        reads_per_minute: READS_PER_MINUTE_CAP,
        reserved_resolution_per_second: RESERVED_RESOLUTION_PER_SECOND,
        worst_case_reads_per_minute: worst_case_reads_const(),
    }
}

/// Re-export the scheduler's worst-case reads/minute so callers that hold only
/// the budget can bind their occupancy math to the same number.
#[must_use]
pub fn worst_case_reads_per_minute() -> u64 {
    poll_tiers::worst_case_reads_per_minute()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oms::groww::poll_tiers::{TierEligibility, assign_poll_tiers, reads_per_minute};

    #[test]
    fn self_caps_are_at_or_below_doc_ceilings() {
        let d = describe_budget();
        assert!(d.orders_per_second <= DOC_ORDERS_PER_SECOND);
        assert!(d.orders_per_minute <= DOC_ORDERS_PER_MINUTE);
        assert!(d.reads_per_second <= DOC_READS_PER_SECOND);
        assert!(d.reads_per_minute <= DOC_READS_PER_MINUTE);
        // ≤50% orders, ≤40% reads (design §4.8).
        assert!(d.orders_per_second * 2 <= DOC_ORDERS_PER_SECOND);
        assert!(d.reads_per_minute * 5 <= DOC_READS_PER_MINUTE * 2);
    }

    #[test]
    fn mutation_budget_allows_then_denies_within_one_second() {
        let b = GrowwRateBudget::new();
        // The per-second burst is the binding cap in a tight loop.
        let mut allowed = 0_u32;
        for _ in 0..(ORDERS_PER_SECOND_CAP + 5) {
            if b.check_mutation().is_allowed() {
                allowed += 1;
            }
        }
        // Burst-bounded: no more than the per-second cap in the immediate burst.
        assert!(allowed <= ORDERS_PER_SECOND_CAP, "allowed {allowed}");
        assert!(allowed >= 1);
    }

    #[test]
    fn resolution_reads_outrank_routine_via_reserved_slice() {
        let b = GrowwRateBudget::new();
        // Drain the shared reads per-second burst with routine reads.
        for _ in 0..(READS_PER_SECOND_CAP + 4) {
            let _ = b.check_routine_read();
        }
        // The reserved 1/s slice still grants a resolution read this second.
        assert_eq!(b.check_resolution_read(), ReadSlot::Reserved);
    }

    #[test]
    fn routine_reads_are_burst_bounded() {
        let b = GrowwRateBudget::new();
        let mut allowed = 0_u32;
        for _ in 0..(READS_PER_SECOND_CAP + 6) {
            if b.check_routine_read().is_allowed() {
                allowed += 1;
            }
        }
        assert!(allowed <= READS_PER_SECOND_CAP, "allowed {allowed}");
    }

    /// The rate-budget saturation contract, tied to the poll-tier scheduler
    /// (reuse the poll_tiers occupancy model — design §4.8 / hostile F-8):
    /// under FULL 8-order occupancy the scheduler's implied reads/minute stays
    /// ≤ the reads per-minute self-cap.
    #[test]
    fn scheduler_saturation_stays_within_reads_cap() {
        // 8 hot-eligible orders (the worst case: every order wants the 2s tier).
        let orders = [TierEligibility {
            hot_eligible: true,
            warm_eligible: true,
        }; 8];
        let assign = assign_poll_tiers(&orders).expect("8 == MAX_TRACKED");
        let implied = reads_per_minute(&assign);
        assert!(
            implied <= u64::from(READS_PER_MINUTE_CAP),
            "scheduler implied {implied}/min exceeds reads cap {READS_PER_MINUTE_CAP}"
        );
        // And the standalone worst-case number matches the descriptor.
        assert_eq!(
            worst_case_reads_per_minute(),
            describe_budget().worst_case_reads_per_minute
        );
        assert!(worst_case_reads_per_minute() <= u64::from(READS_PER_MINUTE_CAP));
    }

    #[test]
    fn family_labels_are_stable() {
        assert_eq!(RateFamily::Orders.as_str(), "orders");
        assert_eq!(RateFamily::Reads.as_str(), "reads");
    }
}
