//! Ranks option contracts by traded volume, with the two option families
//! ranked APART.
//!
//! # Why this exists (2026-09-06)
//!
//! Operator, verbatim: *"for depth 20 check every 5 seconds top volume of
//! stocks options strikes contracts alone dude I mean pick top 250 top volume
//! aligned wirh top gainers"*, then minutes later *"we need to split this top
//! volume gainers purely based on indices options vs stocks options"*.
//!
//! The split is the load-bearing half and it is a CORRECTNESS requirement, not
//! a refinement. Measured on the box 2026-08-22: **1,250 index-option contracts
//! against 20,220 stock-option contracts**, and one NIFTY weekly at-the-money
//! strike out-trades stock-option strikes by orders of magnitude. A single
//! blended top-250 by raw volume therefore returns **250 index strikes and ZERO
//! stock options** — the exact opposite of the requirement, returned silently,
//! with every counter green. Pinned by
//! [`tests::a_blended_ranking_would_return_zero_stock_options`], which exists as
//! the negative control: it proves the split earns its place rather than
//! asserting that it does.
//!
//! # There is no heap and no threshold, and that is the design
//!
//! The first design pitched to the operator kept an incremental min-heap of the
//! top 250 and compared each tick against its weakest member — O(1) per tick,
//! and it sounded right. A hostile review killed it:
//!
//! > One garbage value near `u32::MAX` pins the threshold at a value nothing
//! > can ever beat. The depth set freezes for the rest of the session. No error
//! > fires, no counter moves, every dashboard stays green.
//!
//! A full sweep costs **900 µs** at 20,220 contracts — a **0.018% duty cycle**
//! at the 5-second cadence. The heap was premature optimisation against a cost
//! that measurement shows is negligible, and it bought the single worst failure
//! mode in the design. So
//! this module holds no threshold: [`VolumeLeaderboard::rank`] recomputes from
//! current state, one bad value affects one contract instead of the whole set,
//! and it self-corrects the moment a good value arrives.
//!
//! # The monotonicity gate IS the overflow guard
//!
//! [`tickvault_common::tick_types::ParsedTick::volume`] is a `u32` and the WIRE
//! value wraps at the vendor, which we cannot observe directly. But a wrap
//! manifests as cumulative volume *falling* — from ~4.29e9 to near zero — and
//! so do the other three breakers of monotonicity: write-ahead-log replay
//! re-injecting older frames after newer ones, Dhan skipping a slow consumer
//! forward to "the latest available state" with no sequence number, and the
//! 09:00 session reset. **One gate catches all four — with one hole, stated rather
//! than implied.** A wrap landing exactly on ZERO is invisible here: zero is
//! short-circuited earlier as the pre-open state of every contract and cannot
//! be told apart from it. Unreachable in practice, and recorded because
//! "catches all four" read alone would overstate it. Refusing is the right
//! direction: holding the last good high keeps the most liquid contract ranked,
//! where accepting the wrapped value would silently drop it off the leaderboard.
//!
//! This restores the behaviour `ErrorCode::Volume01MonotonicityBreach` was minted for. Its
//! module was deleted and the error code outlived it, so a wrapped or reset
//! counter has been ranking silently ever since.
//!
//! # This module is pure
//!
//! It takes observations and returns a ranking. It reads no database, opens no
//! socket and performs no I/O, so every edge case below is a unit test rather
//! than a live-session surprise.
//!
//! # Allocation
//!
//! The maps and the sweep buffer are pre-sized at construction and the metric
//! handles are resolved there too, so [`VolumeLeaderboard::observe`] — the
//! per-tick path — allocates nothing in steady state.
//!
//! **`rank_distinct_underlying` DOES allocate**, two `Vec`s of `k` per call. It
//! is the depth-200 path, called once per cadence with `k = 5`, not per tick.
//! Stated because an earlier version of this header said "allocation happens
//! once at construction" without qualification, which was true of `rank` and
//! false of that path.

use std::collections::HashMap;

use tickvault_common::error_code::ErrorCode;
use tickvault_common::types::ExchangeSegment;
use tracing::error;

/// Contracts tracked before the map refuses new ones.
///
/// 25,000 deliberately, matching `AGGREGATOR_MAX_SLOTS`,
/// `MAX_INDICATOR_INSTRUMENTS` and the two per-instrument tracker caps, so one
/// figure covers every per-instrument structure on the same universe. The
/// authorized set measured 22,996 on 2026-08-21, leaving 2,004 spare.
pub const MAX_TRACKED_CONTRACTS: usize = 25_000;

/// Counter: observations refused, labelled by reason.
pub const REFUSED_COUNTER: &str = "tv_volume_leaderboard_refused_total";
/// Gauge: contracts currently tracked, per family.
pub const TRACKED_GAUGE: &str = "tv_volume_leaderboard_tracked";
/// Gauge: size of the last materialised ranking, per family.
pub const RANKED_GAUGE: &str = "tv_volume_leaderboard_ranked";

/// Which option family a contract belongs to.
///
/// The two are ranked in separate leaderboards because their volumes are not
/// comparable — see the module header. This is the same reasoning the depth-200
/// selector already applies one level up, where cross-underlying ranking is
/// normalised rather than raw rupees because a 24,000 index and a 57,000 index
/// are not comparable in absolute terms.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OptionFamily {
    /// `OPTIDX` — NIFTY, BANKNIFTY and the other index chains.
    Index,
    /// `OPTSTK` — the only family depth subscribes as of the 2026-09-06 lock.
    Stock,
}

impl OptionFamily {
    /// Metric label.
    ///
    /// NOTE: being a `&'static str` is NOT what makes a label cheap.
    /// `metrics::counter!` selects its zero-allocation arm on the label value
    /// being a LITERAL; a variable of any type falls through to the arm that
    /// builds a `Vec`. The counters are therefore resolved ONCE into handles on
    /// [`Family`] rather than being re-macro'd per call.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Index => "index",
            Self::Stock => "stock",
        }
    }
}

/// The I-P1-11 composite identity of a contract.
///
/// `security_id` ALONE is not unique — Dhan reuses one numeric id across
/// segments, so id 13 is NIFTY in `IDX_I` and an unrelated cash stock in
/// `NSE_EQ`. Keying on the bare id would silently merge two instruments'
/// volumes into one leaderboard entry.
pub type ContractKey = (u64, ExchangeSegment);

/// One contract's ranking row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RankedContract {
    /// The contract's own Dhan `SecurityId` — not the underlying's.
    pub security_id: u64,
    /// The contract's segment. Part of the identity, never dropped.
    pub segment: ExchangeSegment,
    /// The UNDERLYING's `SecurityId`, carried for the depth-200
    /// distinct-underlying constraint and for gainer eligibility.
    pub underlying_id: u64,
    /// Cumulative traded volume for the day, as last accepted.
    pub volume: u32,
}

/// What happened to one observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Observation {
    /// Recorded. The contract's stored volume advanced.
    Accepted,
    /// The offered volume is not above the stored one, so it was refused and
    /// the stored high retained. A wrap, a replayed frame, a vendor
    /// consumer-skip, or a missed session reset — the gate does not need to
    /// know which.
    RefusedNonMonotonic {
        /// What we already hold for this contract.
        stored: u32,
        /// What arrived.
        offered: u32,
    },
    /// The map is full and this contract is new. Already-tracked contracts are
    /// unaffected; this one will not be ranked.
    RefusedAtCapacity,
    /// Volume is zero, which is the pre-open state of every contract. Not an
    /// error and not counted as a refusal — simply not rankable.
    Ignored,
    /// The contract is tracked and the volume is IDENTICAL to what is stored.
    ///
    /// The most common outcome on a live feed, and not a fault: a packet is
    /// emitted on an LTP, bid, ask or OI change, so most packets for a
    /// contract carry a volume that has not moved since the last one. Never
    /// counted, never logged.
    Unchanged,
}

#[derive(Debug)]
struct Family {
    volumes: HashMap<ContractKey, RankedContract>,
    non_monotonic: u64,
    at_capacity: u64,
    /// PRE-RESOLVED counter handles.
    ///
    /// `metrics::counter!` selects its zero-allocation arm on the label value
    /// being a LITERAL, not on its type -- a `&'static str` held in a variable
    /// still falls through to the arm that builds a `Vec`. CLAUDE.md records
    /// exactly this costing ~36M allocations/hour in `record_ws_lag`, and the
    /// fix there was the same one: resolve the handles once and increment them.
    ///
    /// An earlier doc line here claimed "a `&'static str` so the label never
    /// allocates". That was wrong about the mechanism, and it was wrong on the
    /// per-tick path.
    refused_non_monotonic: metrics::Counter,
    refused_capacity: metrics::Counter,
}

impl Family {
    fn new(family: OptionFamily) -> Self {
        let label = family.as_str();
        // Seeded at zero here: the CloudWatch agent computes a counter as the
        // delta between consecutive samples and DROPS the first sample of a
        // series it has never seen, so a counter whose first increment IS the
        // event publishes nothing on the one day it matters (the 2026-08-28
        // incident that made 104,540 depth rows permanently unclassifiable).
        let refused_non_monotonic =
            metrics::counter!(REFUSED_COUNTER, "family" => label, "reason" => "non_monotonic");
        refused_non_monotonic.increment(0);
        let refused_capacity =
            metrics::counter!(REFUSED_COUNTER, "family" => label, "reason" => "capacity");
        refused_capacity.increment(0);
        Self {
            // Pre-sized: an unsized map reallocates and rehashes ~15 times on
            // its way to the authorized universe, and every one of those lands
            // on the per-tick path.
            volumes: HashMap::with_capacity(MAX_TRACKED_CONTRACTS),
            non_monotonic: 0,
            at_capacity: 0,
            refused_non_monotonic,
            refused_capacity,
        }
    }

    /// Clears the tracked contracts and counters, KEEPING the map capacity and
    /// the resolved handles. Replacing the struct would hand back the capacity
    /// and pay the growth again every session.
    fn clear(&mut self) {
        self.volumes.clear();
        self.non_monotonic = 0;
        self.at_capacity = 0;
    }
}

/// Per-family volume leaderboards.
#[derive(Debug)]
pub struct VolumeLeaderboard {
    index: Family,
    stock: Family,
    /// Reusable sweep buffer. Sized once at construction so [`Self::rank`]
    /// allocates nothing, however often it runs.
    scratch: Vec<RankedContract>,
}

impl Default for VolumeLeaderboard {
    fn default() -> Self {
        Self::new()
    }
}

impl VolumeLeaderboard {
    /// Builds a leaderboard, pre-sizing the sweep buffer.
    #[must_use]
    pub fn new() -> Self {
        // `Family::new` resolves and seeds each family's counter handles.
        Self {
            index: Family::new(OptionFamily::Index),
            stock: Family::new(OptionFamily::Stock),
            scratch: Vec::with_capacity(MAX_TRACKED_CONTRACTS),
        }
    }

    const fn family_mut(&mut self, family: OptionFamily) -> &mut Family {
        match family {
            OptionFamily::Index => &mut self.index,
            OptionFamily::Stock => &mut self.stock,
        }
    }

    const fn family_ref(&self, family: OptionFamily) -> &Family {
        match family {
            OptionFamily::Index => &self.index,
            OptionFamily::Stock => &self.stock,
        }
    }

    /// Records one tick's cumulative volume. **This is the whole per-tick path.**
    ///
    /// One hash lookup and one comparison. No heap, no threshold, no
    /// allocation.
    pub fn observe(&mut self, contract: RankedContract, family: OptionFamily) -> Observation {
        // Zero is the pre-open state of every contract, and ranking an
        // all-zero field would make the depth set "whichever 250 ticked
        // first" — arbitrary, and then a total turnover at the bell. Excluded
        // here rather than filtered later, which also means the tie policy
        // never has to decide a live set.
        if contract.volume == 0 {
            return Observation::Ignored;
        }
        let key = (contract.security_id, contract.segment);
        let label = family.as_str();
        let slot = self.family_mut(family);

        if let Some(existing) = slot.volumes.get_mut(&key) {
            // STRICTLY less. An UNCHANGED cumulative volume is the NORMAL state,
            // not a fault: a Full/Quote packet is emitted on an LTP, bid, ask
            // or OI change, so between two trades of one contract many packets
            // carry the same volume. Treating equal as a breach made the
            // overwhelmingly common tick a refusal -- which both swamped the
            // ONE counter that detects a real wrap and emitted a coded error
            // whose text says volume went backwards when it had not moved.
            if contract.volume < existing.volume {
                let stored = existing.volume;
                slot.non_monotonic = slot.non_monotonic.saturating_add(1);
                let seen = slot.non_monotonic;
                slot.refused_non_monotonic.increment(1);
                // Powers of two, so a storm cannot flood the sink. A wrapped
                // contract refuses on every subsequent tick for the rest of the
                // session, which is thousands of events for one condition.
                if seen.is_power_of_two() {
                    error!(
                        code = ErrorCode::Volume01MonotonicityBreach.code_str(),
                        family = label,
                        security_id = contract.security_id,
                        ?contract.segment,
                        stored,
                        offered = contract.volume,
                        refused_total = seen,
                        "cumulative volume did not advance — refusing and keeping the stored \
                         high. A wrapped 32-bit counter, a replayed frame, a vendor \
                         consumer-skip and a missed session reset all look like this. The \
                         contract stays ranked at its last good volume rather than dropping to \
                         near-zero. Throttled to powers of two per family."
                    );
                }
                return Observation::RefusedNonMonotonic {
                    stored,
                    offered: contract.volume,
                };
            }
            if contract.volume == existing.volume {
                // Nothing to record and nothing wrong. Deliberately NOT counted
                // and NOT logged.
                return Observation::Unchanged;
            }
            // Advance in place. The underlying is refreshed too: a derivative
            // id can be reused across days, and holding a stale underlying
            // would put the contract under the wrong name in the depth-200
            // distinct-underlying constraint.
            *existing = contract;
            return Observation::Accepted;
        }

        if slot.volumes.len() >= MAX_TRACKED_CONTRACTS {
            slot.at_capacity = slot.at_capacity.saturating_add(1);
            let seen = slot.at_capacity;
            slot.refused_capacity.increment(1);
            if seen.is_power_of_two() {
                error!(
                    code = ErrorCode::Volume01MonotonicityBreach.code_str(),
                    family = label,
                    security_id = contract.security_id,
                    ?contract.segment,
                    tracked = slot.volumes.len(),
                    max = MAX_TRACKED_CONTRACTS,
                    refused_total = seen,
                    "volume leaderboard is FULL — refusing a NEW contract; it will never be \
                     ranked and so can never take a depth slot. Already-tracked contracts are \
                     unaffected. Fail-closed on the insert path, never an eviction."
                );
            }
            return Observation::RefusedAtCapacity;
        }

        slot.volumes.insert(key, contract);
        Observation::Accepted
    }

    /// Materialises the top `k` of one family, highest volume first.
    ///
    /// `eligible` decides membership; volume decides ORDER. That split is what
    /// keeps the ordered set monotonic and therefore stable, which is what
    /// keeps the re-subscribe delta small enough to be affordable — see the
    /// 2026-09-06 scope-lock section.
    ///
    /// Returns an EMPTY slice when nothing qualifies. The caller must hold its
    /// previous layout rather than subscribing an empty set: a depth pool
    /// reported as enabled while carrying nothing is the false-OK the scope
    /// lock bans.
    pub fn rank<F>(&mut self, family: OptionFamily, k: usize, eligible: F) -> &[RankedContract]
    where
        F: Fn(&RankedContract) -> bool,
    {
        // `take` rather than a direct fill: the buffer and the family map both
        // live on `self`, so filling one from the other needs two borrows. Take
        // moves the Vec out WITH its capacity, so the allocation is still made
        // once at construction and reused for the life of the process.
        let mut scratch = std::mem::take(&mut self.scratch);
        scratch.clear();
        scratch.extend(
            self.family_ref(family)
                .volumes
                .values()
                .copied()
                .filter(&eligible),
        );

        // Highest volume first, then a DETERMINISTIC tiebreak on the composite
        // identity. Without the tiebreak, equal volumes order by whatever the
        // hash map yielded, so the same input reorders between sweeps and the
        // caller swaps subscriptions for nothing.
        scratch.sort_unstable_by(|a, b| {
            b.volume
                .cmp(&a.volume)
                .then_with(|| a.security_id.cmp(&b.security_id))
                .then_with(|| (a.segment as u8).cmp(&(b.segment as u8)))
        });
        scratch.truncate(k);
        self.scratch = scratch;

        metrics::gauge!(TRACKED_GAUGE, "family" => family.as_str())
            .set(self.family_ref(family).volumes.len() as f64);
        metrics::gauge!(RANKED_GAUGE, "family" => family.as_str()).set(self.scratch.len() as f64);
        &self.scratch
    }

    /// Materialises the top `k`, at most ONE contract per underlying.
    ///
    /// The depth-200 rule: operator, 2026-09-06, *"ensure on depth 200 top 5
    /// should never ever be same symbols strike"*.
    ///
    /// Greedy over the volume order, so each underlying is represented by its
    /// heaviest contract. **The honest cost, which the scope lock records:**
    /// when the true top five are five strikes of one stock, this forces four
    /// substitutions into thinner books — the shape that produced 800
    /// rows/minute against 100,800 on 2026-08-26.
    pub fn rank_distinct_underlying<F>(
        &mut self,
        family: OptionFamily,
        k: usize,
        eligible: F,
    ) -> Vec<RankedContract>
    where
        F: Fn(&RankedContract) -> bool,
    {
        // CLAMPED before the allocations below. `Vec::with_capacity(k)` PANICS
        // on a capacity overflow, and the release profile is `panic = "abort"`,
        // so an unclamped caller-supplied `k` is a process abort rather than a
        // bad ranking. `rank` is already safe by construction because
        // `truncate` clamps; this path allocates first, so it must clamp first.
        let k = k.min(MAX_TRACKED_CONTRACTS);
        let ordered = self.rank(family, MAX_TRACKED_CONTRACTS, eligible);
        let mut seen: Vec<u64> = Vec::with_capacity(k);
        let mut out: Vec<RankedContract> = Vec::with_capacity(k);
        for row in ordered {
            if out.len() >= k {
                break;
            }
            if seen.contains(&row.underlying_id) {
                continue;
            }
            seen.push(row.underlying_id);
            out.push(*row);
        }
        // `rank` above set this to the INTERMEDIATE sweep length (up to the
        // whole map), because that is what it materialised. What this path
        // actually hands the caller is `out`, and a gauge documented as "the
        // size of the last materialised ranking" reporting 20,000 when five
        // sockets were filled is the wrong number for the only question an
        // operator asks of it.
        metrics::gauge!(RANKED_GAUGE, "family" => family.as_str()).set(out.len() as f64);
        out
    }

    /// Contracts currently tracked for a family.
    #[must_use]
    pub fn tracked(&self, family: OptionFamily) -> usize {
        self.family_ref(family).volumes.len()
    }

    /// Observations refused as non-monotonic, for a family.
    #[must_use]
    pub const fn non_monotonic_refusals(&self, family: OptionFamily) -> u64 {
        self.family_ref(family).non_monotonic
    }

    /// Observations refused because the map was full, for a family.
    #[must_use]
    pub const fn capacity_refusals(&self, family: OptionFamily) -> u64 {
        self.family_ref(family).at_capacity
    }

    /// Clears both families at the session boundary.
    ///
    /// Cumulative volume restarts at 09:00, so a leaderboard carried across the
    /// boundary would refuse every contract's first tick as non-monotonic and
    /// then rank on YESTERDAY for the whole day — a failure the gate itself
    /// would mask, which is why the reset is asserted by a test rather than
    /// assumed.
    pub fn reset_daily(&mut self) {
        self.index.clear();
        self.stock.clear();
        self.scratch.clear();
    }
}

/// Percent change against the previous close, or `None` when it cannot be
/// computed honestly.
///
/// **The previous close is a PROVEN source of not-a-number** — the quote parser
/// carries a test that asserts `tick.day_close.is_nan()` — and `0.0` is a live
/// sentinel for Ticker-mode packets and pre-open instruments. A non-finite
/// value reaching a sort comparator is worse than one bad row: the comparison
/// is non-transitive, so the ORDERING corrupts wholesale.
///
/// This is the §28.4 poisoning class one module over, gated at ingest rather
/// than clamped at the output — clamping is what let a NaN reach indicator
/// state and sit there for the life of the process.
#[must_use]
pub fn eligible_gain_pct(ltp: f64, prev_close: f64) -> Option<f64> {
    if !ltp.is_finite() || !prev_close.is_finite() || ltp <= 0.0 || prev_close <= 0.0 {
        return None;
    }
    let pct = (ltp - prev_close) / prev_close * 100.0;
    pct.is_finite().then_some(pct)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stock(id: u64, underlying: u64, volume: u32) -> RankedContract {
        RankedContract {
            security_id: id,
            segment: ExchangeSegment::NseFno,
            underlying_id: underlying,
            volume,
        }
    }

    fn all(_: &RankedContract) -> bool {
        true
    }

    #[test]
    fn monotonic_volume_updates_are_accepted_and_ranked() {
        let mut lb = VolumeLeaderboard::new();
        assert_eq!(
            lb.observe(stock(1, 100, 500), OptionFamily::Stock),
            Observation::Accepted
        );
        assert_eq!(
            lb.observe(stock(1, 100, 900), OptionFamily::Stock),
            Observation::Accepted
        );
        assert_eq!(
            lb.observe(stock(2, 200, 700), OptionFamily::Stock),
            Observation::Accepted
        );
        let ranked = lb.rank(OptionFamily::Stock, 10, all);
        assert_eq!(ranked.len(), 2);
        assert_eq!(ranked[0].security_id, 1, "900 must outrank 700");
        assert_eq!(
            ranked[0].volume, 900,
            "the LATEST volume must be the ranked one"
        );
        assert_eq!(ranked[1].security_id, 2);
    }

    #[test]
    fn non_monotonic_refusals_retain_the_stored_high_when_volume_falls() {
        // The whole point of the gate. Accepting the lower value would drop the
        // most liquid contract to near-zero and off the leaderboard entirely.
        let mut lb = VolumeLeaderboard::new();
        lb.observe(stock(1, 100, 4_000_000_000), OptionFamily::Stock);
        assert_eq!(
            lb.observe(stock(1, 100, 12), OptionFamily::Stock),
            Observation::RefusedNonMonotonic {
                stored: 4_000_000_000,
                offered: 12
            }
        );
        let ranked = lb.rank(OptionFamily::Stock, 10, all);
        assert_eq!(
            ranked[0].volume, 4_000_000_000,
            "the stored high must survive the refusal"
        );
        assert_eq!(lb.non_monotonic_refusals(OptionFamily::Stock), 1);
    }

    #[test]
    fn a_wrapped_counter_is_refused_by_the_same_gate_that_catches_replay() {
        // ParsedTick.volume is u32 and the WIRE value wraps at the vendor. We
        // never see the wrap; we see the fall. One gate, four causes: a wrap, a
        // WAL replay of an older frame, a vendor consumer-skip, and the 09:00
        // reset. This test is the wrap; the one above is the general case.
        let mut lb = VolumeLeaderboard::new();
        lb.observe(stock(1, 100, u32::MAX - 5), OptionFamily::Stock);
        // u32::MAX - 5 plus 10 wraps to 4 on the wire.
        let wrapped = (u32::MAX - 5).wrapping_add(10);
        assert_eq!(wrapped, 4, "sanity: this is what a wrap looks like");
        assert!(matches!(
            lb.observe(stock(1, 100, wrapped), OptionFamily::Stock),
            Observation::RefusedNonMonotonic { .. }
        ));
        assert_eq!(
            lb.rank(OptionFamily::Stock, 1, all)[0].volume,
            u32::MAX - 5,
            "a wrapped contract stays ranked at its last good high, not at 4"
        );
    }

    #[test]
    fn an_equal_volume_is_unchanged_not_a_breach_and_never_touches_the_counter() {
        // CORRECTED 2026-09-06. This test previously asserted the opposite,
        // under the name `an_equal_volume_is_refused_so_a_repeated_tick_cannot
        // _churn_the_order` -- and BOTH halves of that name were wrong.
        //
        // It cannot churn the order: the sort is deterministic on
        // (volume, security_id, segment), so re-storing an identical volume
        // reorders nothing. Refusing it bought no stability at all.
        //
        // And it is not a breach. A Full/Quote packet is emitted on an LTP,
        // bid, ask or OI change -- NOT only on a trade -- so between two trades
        // of one contract, most packets carry a volume that has not moved.
        // Counting those as non-monotonic made the ordinary tick a refusal,
        // which swamped the one counter that detects a real 32-bit wrap and
        // emitted a coded error whose text says volume went backwards when it
        // had not moved at all.
        let mut lb = VolumeLeaderboard::new();
        lb.observe(stock(1, 100, 500), OptionFamily::Stock);

        for _ in 0..1_000 {
            assert_eq!(
                lb.observe(stock(1, 100, 500), OptionFamily::Stock),
                Observation::Unchanged,
                "an unmoved cumulative volume is the normal tick, not a fault"
            );
        }
        assert_eq!(
            lb.non_monotonic_refusals(OptionFamily::Stock),
            0,
            "a thousand ordinary ticks must leave the wrap detector at zero"
        );

        // And a genuine decrease still refuses, so the gate is not simply gone.
        assert!(matches!(
            lb.observe(stock(1, 100, 499), OptionFamily::Stock),
            Observation::RefusedNonMonotonic { .. }
        ));
        assert_eq!(lb.non_monotonic_refusals(OptionFamily::Stock), 1);
        assert_eq!(
            lb.rank(OptionFamily::Stock, 1, all)[0].volume,
            500,
            "and the stored high survives the refusal"
        );
    }

    #[test]
    fn zero_volume_is_ignored_rather_than_refused() {
        // Pre-open state of every contract. Not an error, so it must not
        // inflate the refusal counter that a future alarm would read.
        let mut lb = VolumeLeaderboard::new();
        assert_eq!(
            lb.observe(stock(1, 100, 0), OptionFamily::Stock),
            Observation::Ignored
        );
        assert_eq!(lb.non_monotonic_refusals(OptionFamily::Stock), 0);
        assert_eq!(lb.tracked(OptionFamily::Stock), 0);
    }

    #[test]
    fn top_k_returns_empty_when_every_volume_is_zero() {
        // The pre-open rule. An empty ranking is the signal for the caller to
        // HOLD its previous layout; subscribing an arbitrary set here is what
        // makes the depth pool churn completely at the bell.
        let mut lb = VolumeLeaderboard::new();
        for id in 0..50u64 {
            lb.observe(stock(id, id, 0), OptionFamily::Stock);
        }
        assert!(
            lb.rank(OptionFamily::Stock, 250, all).is_empty(),
            "an all-zero field must rank nothing, never an arbitrary 250"
        );
    }

    #[test]
    fn index_and_stock_options_are_ranked_in_separate_leaderboards() {
        let mut lb = VolumeLeaderboard::new();
        lb.observe(stock(1, 100, 900), OptionFamily::Stock);
        lb.observe(stock(2, 200, 5_000_000), OptionFamily::Index);
        assert_eq!(lb.tracked(OptionFamily::Stock), 1);
        assert_eq!(lb.tracked(OptionFamily::Index), 1);
        assert_eq!(lb.rank(OptionFamily::Stock, 10, all).len(), 1);
        assert_eq!(lb.rank(OptionFamily::Stock, 10, all)[0].security_id, 1);
        assert_eq!(lb.rank(OptionFamily::Index, 10, all)[0].security_id, 2);
    }

    #[test]
    fn a_blended_ranking_would_return_zero_stock_options() {
        // THE NEGATIVE CONTROL. This proves the split earns its place rather
        // than asserting that it does.
        //
        // Measured on the box 2026-08-22: 1,250 index-option contracts against
        // 20,220 stock-option contracts, and one NIFTY weekly at-the-money
        // strike out-trades stock-option strikes by orders of magnitude. The
        // shape below is that, scaled down.
        let mut lb = VolumeLeaderboard::new();
        let mut blended: Vec<RankedContract> = Vec::new();

        for id in 0..300u64 {
            let c = stock(id, id % 40, 10_000_000 + id as u32);
            lb.observe(c, OptionFamily::Index);
            blended.push(c);
        }
        for id in 1_000..1_300u64 {
            let c = stock(id, id % 40, 5_000 + id as u32);
            lb.observe(c, OptionFamily::Stock);
            blended.push(c);
        }

        blended.sort_unstable_by(|a, b| b.volume.cmp(&a.volume));
        let stock_in_blended_top_250 = blended
            .iter()
            .take(250)
            .filter(|c| c.security_id >= 1_000)
            .count();
        assert_eq!(
            stock_in_blended_top_250, 0,
            "a blended top-250 returns ZERO stock options — the exact opposite \
             of the requirement, returned silently. This is why the families are \
             ranked apart."
        );

        assert_eq!(
            lb.rank(OptionFamily::Stock, 250, all).len(),
            250,
            "ranked apart, the stock family fills all 250 slots"
        );
    }

    #[test]
    fn capacity_refusals_refuse_new_contracts_but_keep_serving_the_tracked_ones() {
        let mut lb = VolumeLeaderboard::new();
        for id in 0..MAX_TRACKED_CONTRACTS as u64 {
            assert_eq!(
                lb.observe(stock(id, id, 1_000), OptionFamily::Stock),
                Observation::Accepted
            );
        }
        assert_eq!(
            lb.observe(stock(999_999, 1, 5_000), OptionFamily::Stock),
            Observation::RefusedAtCapacity
        );
        assert_eq!(lb.capacity_refusals(OptionFamily::Stock), 1);
        // Fail-closed on the INSERT path only — an already-tracked contract is
        // never evicted and keeps advancing.
        assert_eq!(
            lb.observe(stock(0, 0, 9_999), OptionFamily::Stock),
            Observation::Accepted
        );
        assert_eq!(lb.tracked(OptionFamily::Stock), MAX_TRACKED_CONTRACTS);
    }

    #[test]
    fn ties_break_deterministically_on_the_composite_key() {
        // Without this the order comes from whatever the hash map yielded, so
        // the same input reorders between sweeps and the caller swaps
        // subscriptions for nothing.
        let mut lb = VolumeLeaderboard::new();
        for id in (0..40u64).rev() {
            lb.observe(stock(id, id, 777), OptionFamily::Stock);
        }
        let first: Vec<u64> = lb
            .rank(OptionFamily::Stock, 40, all)
            .iter()
            .map(|c| c.security_id)
            .collect();
        let second: Vec<u64> = lb
            .rank(OptionFamily::Stock, 40, all)
            .iter()
            .map(|c| c.security_id)
            .collect();
        assert_eq!(first, second, "two sweeps of identical state must agree");
        assert_eq!(first[0], 0, "the tiebreak is ascending on security_id");
        assert_eq!(first[39], 39);
    }

    #[test]
    fn eligibility_filters_membership_while_volume_decides_order() {
        let mut lb = VolumeLeaderboard::new();
        lb.observe(stock(1, 100, 900), OptionFamily::Stock);
        lb.observe(stock(2, 200, 800), OptionFamily::Stock);
        lb.observe(stock(3, 300, 700), OptionFamily::Stock);
        let ranked = lb.rank(OptionFamily::Stock, 10, |c| c.underlying_id != 100);
        assert_eq!(ranked.len(), 2, "underlying 100 is filtered out");
        assert_eq!(ranked[0].security_id, 2, "volume still decides the order");
        assert_eq!(ranked[1].security_id, 3);
    }

    #[test]
    fn rank_distinct_underlying_takes_the_heaviest_contract_per_name() {
        // The depth-200 rule. Five strikes of one stock become one entry, and
        // the remaining four slots go to the next four names.
        let mut lb = VolumeLeaderboard::new();
        for strike in 0..5u64 {
            lb.observe(
                stock(strike, 100, 9_000 - strike as u32),
                OptionFamily::Stock,
            );
        }
        for name in 1..5u64 {
            lb.observe(stock(100 + name, 100 + name, 1_000), OptionFamily::Stock);
        }
        let picked = lb.rank_distinct_underlying(OptionFamily::Stock, 5, all);
        assert_eq!(picked.len(), 5);
        assert_eq!(
            picked[0].security_id, 0,
            "the heaviest strike of the heaviest name is kept"
        );
        let names: Vec<u64> = picked.iter().map(|c| c.underlying_id).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), 5, "all five underlyings must be distinct");
    }

    #[test]
    fn rank_distinct_underlying_returns_fewer_than_k_rather_than_repeating_a_name() {
        // Honest under-fill. Returning four when only four names exist is
        // correct; padding with a second strike of a name would break the
        // operator's rule silently.
        let mut lb = VolumeLeaderboard::new();
        for strike in 0..20u64 {
            lb.observe(
                stock(strike, strike % 4, 5_000 + strike as u32),
                OptionFamily::Stock,
            );
        }
        let picked = lb.rank_distinct_underlying(OptionFamily::Stock, 5, all);
        assert_eq!(picked.len(), 4, "only four distinct underlyings exist");
    }

    #[test]
    fn multiple_strikes_of_one_symbol_collapse_to_one_and_the_next_names_fill_in() {
        // Operator, 2026-09-06 (fourth quote): "If same symbols multiple
        // strikes contracts means then have it as unique".
        //
        // The sibling test above proves the RESULT is five distinct names.
        // This one proves the SUBSTITUTION RULE: the four displaced strikes are
        // replaced by the next-heaviest DISTINCT names in volume order, not by
        // an arbitrary four. That is the half a distinctness assertion cannot
        // see -- a selector that returned the five lightest distinct names
        // would satisfy "all distinct" and be wrong.
        let mut lb = VolumeLeaderboard::new();

        // Name 100 owns the raw top five outright: five strikes, 9_000..8_996.
        for strike in 0..5u64 {
            lb.observe(
                stock(strike, 100, 9_000 - strike as u32),
                OptionFamily::Stock,
            );
        }
        // Six other names, DESCENDING, so "next in volume order" has a wrong
        // answer available if the greedy walk ever loses the ordering.
        for (rank, volume) in [8_000u32, 7_000, 6_000, 5_000, 4_000, 3_000]
            .into_iter()
            .enumerate()
        {
            let name = 200 + rank as u64;
            lb.observe(stock(1_000 + name, name, volume), OptionFamily::Stock);
        }

        let picked = lb.rank_distinct_underlying(OptionFamily::Stock, 5, all);

        assert_eq!(picked.len(), 5);
        assert_eq!(
            picked.iter().map(|c| c.underlying_id).collect::<Vec<_>>(),
            vec![100, 200, 201, 202, 203],
            "one entry for the dominant name, then the next four names by volume"
        );
        assert_eq!(
            picked[0].security_id, 0,
            "and the entry kept for the dominant name is its HEAVIEST strike"
        );
        assert!(
            picked
                .iter()
                .all(|c| c.underlying_id != 204 && c.underlying_id != 205),
            "the two lightest names must not displace a heavier one"
        );
    }

    #[test]
    fn an_index_option_can_never_reach_a_stock_ranking() {
        // Operator, 2026-09-06 (fourth quote): "for depth 20 and depth 200
        // never ever use the index options".
        //
        // Depth reads the STOCK family only, so the guarantee is that no
        // observation filed as Index is reachable through the stock rankings --
        // including when the index contract is orders of magnitude heavier,
        // which is the realistic case (one NIFTY weekly ATM strike outtrades
        // stock-option strikes by orders of magnitude).
        let mut lb = VolumeLeaderboard::new();
        for id in 0..5u64 {
            lb.observe(
                stock(id, 900 + id, u32::MAX - id as u32),
                OptionFamily::Index,
            );
        }
        lb.observe(stock(50, 100, 1), OptionFamily::Stock);

        let top = lb.rank(OptionFamily::Stock, 250, all);
        assert_eq!(top.len(), 1, "only the one stock contract is rankable");
        assert_eq!(top[0].security_id, 50);

        let five = lb.rank_distinct_underlying(OptionFamily::Stock, 5, all);
        assert_eq!(five.len(), 1, "the depth-200 path sees the same one");
        assert_eq!(five[0].security_id, 50);
    }

    /// MEASURES the real sweep cost at the authorized ceiling.
    ///
    /// `#[ignore]`d deliberately, the same shape as
    /// `tick_gap_detector::scan_silence_sweep_cost_at_the_authorized_ceiling`
    /// and `multi_tf_aggregator::catch_up_seal_all_sweep_cost_at_the_authorized_ceiling`:
    /// a wall-clock number must never become a flaky CI gate.
    ///
    /// # Why this exists
    ///
    /// The design note that killed the incremental-heap alternative costed this
    /// sweep at "~60 us, from the measured 2.8 ns/instrument constant". That
    /// constant is `scan_silence`'s, and `scan_silence` is a LINEAR compare over
    /// a slot array. This is a `sort_unstable_by` over the filtered set --
    /// O(n log n) with a three-key comparator -- so the transfer was invalid and
    /// the figure was an underestimate. Run this instead of quoting it.
    #[test]
    #[ignore = "wall-clock measurement, not a gate"]
    fn rank_sweep_cost_at_the_authorized_ceiling() {
        // The measured 2026-08-22 stock-option universe.
        const CONTRACTS: u64 = 20_220;
        const DEPTH_20: usize = 250;

        let mut lb = VolumeLeaderboard::new();
        for id in 0..CONTRACTS {
            // Volumes deliberately NOT pre-sorted: a sort's cost depends on the
            // input order, and a pre-sorted input measures the best case.
            let volume = ((id.wrapping_mul(2_654_435_761)) % 5_000_000) as u32 + 1;
            lb.observe(stock(id, id % 220, volume), OptionFamily::Stock);
        }
        assert_eq!(lb.tracked(OptionFamily::Stock), CONTRACTS as usize);

        // Warm, so the first-call map/vec growth is not in the number.
        let _ = lb.rank(OptionFamily::Stock, DEPTH_20, all);

        const ROUNDS: u32 = 50;
        let start = std::time::Instant::now();
        for _ in 0..ROUNDS {
            let top = lb.rank(OptionFamily::Stock, DEPTH_20, all);
            std::hint::black_box(top.len());
        }
        let per_sweep = start.elapsed() / ROUNDS;

        // The depth-200 path sorts the WHOLE map, not the top 250.
        let start5 = std::time::Instant::now();
        for _ in 0..ROUNDS {
            let five = lb.rank_distinct_underlying(OptionFamily::Stock, 5, all);
            std::hint::black_box(five.len());
        }
        let per_distinct = start5.elapsed() / ROUNDS;

        // 5-second cadence.
        let duty = per_sweep.as_secs_f64() / 5.0 * 100.0;
        let duty5 = per_distinct.as_secs_f64() / 5.0 * 100.0;
        println!(
            "MEASURED at {CONTRACTS} contracts:\n  \
             rank(top {DEPTH_20})        = {per_sweep:?}  -> {duty:.4}% duty at 5s\n  \
             rank_distinct_underlying(5) = {per_distinct:?}  -> {duty5:.4}% duty at 5s"
        );

        // Not a gate on the number -- a gate on the SHAPE. A sweep that took a
        // whole cadence would starve the drain arm it shares a task with.
        assert!(
            per_sweep < std::time::Duration::from_secs(1),
            "a sweep must not approach the 5s cadence: {per_sweep:?}"
        );
    }

    #[test]
    fn reset_daily_clears_both_families() {
        // Cumulative volume restarts at 09:00. A leaderboard carried across the
        // boundary refuses every first tick as non-monotonic and then ranks on
        // YESTERDAY all day — a failure the gate itself would mask, which is
        // why this is asserted rather than assumed.
        let mut lb = VolumeLeaderboard::new();
        lb.observe(stock(1, 100, 900), OptionFamily::Stock);
        lb.observe(stock(2, 200, 900), OptionFamily::Index);
        lb.observe(stock(1, 100, 5), OptionFamily::Stock);
        assert_eq!(lb.non_monotonic_refusals(OptionFamily::Stock), 1);

        lb.reset_daily();

        assert_eq!(lb.tracked(OptionFamily::Stock), 0);
        assert_eq!(lb.tracked(OptionFamily::Index), 0);
        assert_eq!(lb.non_monotonic_refusals(OptionFamily::Stock), 0);
        assert_eq!(
            lb.observe(stock(1, 100, 5), OptionFamily::Stock),
            Observation::Accepted,
            "after a reset, yesterday's high must not refuse today's first tick"
        );
    }

    #[test]
    fn eligible_gain_pct_refuses_a_non_finite_or_zero_previous_close() {
        // The quote parser has a test that ASSERTS day_close is NaN, and 0.0 is
        // a live sentinel. A non-finite value in a sort comparator is
        // non-transitive, so the ORDERING corrupts wholesale rather than one
        // row. Gated at ingest, never clamped at the output — clamping is what
        // let a NaN sit in indicator state for the life of the process.
        assert_eq!(eligible_gain_pct(110.0, 100.0), Some(10.0));
        assert_eq!(eligible_gain_pct(90.0, 100.0), Some(-10.0));
        assert_eq!(eligible_gain_pct(100.0, f64::NAN), None, "NaN prev close");
        assert_eq!(eligible_gain_pct(f64::NAN, 100.0), None, "NaN ltp");
        assert_eq!(eligible_gain_pct(100.0, 0.0), None, "the zero sentinel");
        assert_eq!(eligible_gain_pct(0.0, 100.0), None, "zero ltp");
        assert_eq!(eligible_gain_pct(100.0, -1.0), None, "negative prev close");
        assert_eq!(eligible_gain_pct(f64::INFINITY, 100.0), None);
        assert_eq!(eligible_gain_pct(100.0, f64::INFINITY), None);
    }

    #[test]
    fn the_composite_key_keeps_two_segments_apart() {
        // security_id ALONE is not unique — id 13 is NIFTY in IDX_I and an
        // unrelated cash stock in NSE_EQ. Keying on the bare id would merge two
        // instruments' volumes into one leaderboard entry.
        let mut lb = VolumeLeaderboard::new();
        lb.observe(
            RankedContract {
                security_id: 13,
                segment: ExchangeSegment::NseFno,
                underlying_id: 1,
                volume: 500,
            },
            OptionFamily::Stock,
        );
        lb.observe(
            RankedContract {
                security_id: 13,
                segment: ExchangeSegment::BseFno,
                underlying_id: 2,
                volume: 400,
            },
            OptionFamily::Stock,
        );
        assert_eq!(
            lb.tracked(OptionFamily::Stock),
            2,
            "same numeric id, different segments, must be TWO entries"
        );
    }

    #[test]
    fn rank_truncates_to_k_and_keeps_the_heaviest() {
        let mut lb = VolumeLeaderboard::new();
        for id in 0..500u64 {
            lb.observe(stock(id, id, 1_000 + id as u32), OptionFamily::Stock);
        }
        let ranked = lb.rank(OptionFamily::Stock, 250, all);
        assert_eq!(ranked.len(), 250);
        assert_eq!(ranked[0].volume, 1_499, "the heaviest is first");
        assert_eq!(ranked[249].volume, 1_250, "the 250th, not the lightest");
    }
}
