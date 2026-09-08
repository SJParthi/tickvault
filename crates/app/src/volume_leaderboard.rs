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
//! current state, and one bad value affects one contract instead of the whole
//! set.
//!
//! **CORRECTED 2026-09-06, and the correction is the point.** This paragraph
//! used to end "and it self-corrects the moment a good value arrives". That was
//! FALSE, and false in the reassuring direction. The monotonicity gate below
//! refuses every value beneath the stored one, so a contract latched near
//! `u32::MAX` refused every later observation for the rest of the session and
//! held its depth slot until the process restarted. The failure this header
//! quotes a hostile reviewer killing the heap over — *a value nothing can ever
//! beat* — had been reproduced one level down, per contract instead of per set,
//! inside the very design that replaced it. A reader auditing this module for
//! that shape would have read the sentence, believed it, and stopped.
//!
//! [`RELATCH_AFTER_CONSECUTIVE_LOWER`] is what makes the sentence true: after a
//! sustained run of lower observations the stored high is abandoned rather than
//! defended. Pinned by
//! [`tests::a_contract_latched_at_the_ceiling_recovers_instead_of_owning_a_socket_forever`].
//!
//! # The monotonicity gate IS the overflow guard
//!
//! [`tickvault_common::tick_types::ParsedTick::volume`] is a `u32` and the WIRE
//! value wraps at the vendor, which we cannot observe directly. But a wrap
//! manifests as cumulative volume *falling* — from ~4.29e9 to near zero — and
//! so do two of the other three breakers of monotonicity: write-ahead-log
//! replay re-injecting older frames after newer ones, and the 09:00 session
//! reset.
//!
//! **CORRECTED 2026-09-06 — this said "one gate catches all four", and the
//! fourth was never caught.** Dhan skipping a slow consumer forward to "the
//! latest available state" produces a HIGHER cumulative value, not a lower one:
//! the intermediate trades are lost upstream and the next value we see is the
//! new, larger total. That is `Accepted` here and is indistinguishable from
//! ordinary trading — no gate on a monotonicity test can see it, because
//! nothing about it is non-monotonic. The claim was wrong in the reassuring
//! direction: a reader auditing tick-loss detection would have read "covered"
//! and stopped, on the one breaker that is genuinely undetectable from this
//! side of the socket. It stays undetectable; what changes is that this file no
//! longer says otherwise.
//!
//! The remaining hole in what the gate DOES cover: a wrap landing exactly on
//! ZERO is invisible, because zero is short-circuited earlier as the pre-open
//! state of every contract and cannot be told apart from it.
//!
//! Refusing is the right first response — holding the last good high keeps the
//! most liquid contract ranked, where accepting a wrapped value would drop it
//! off the leaderboard — but refusing FOREVER is not, which is what
//! [`RELATCH_AFTER_CONSECUTIVE_LOWER`] exists to bound.
//!
//! **And the premise under all of this is an inference, not a vendor fact.**
//! Checked against the official DhanHQ v2 doc pack on 2026-09-06: the Quote and
//! Full packets document bytes 23-26 as `int32 | Volume` and say nothing more —
//! not "cumulative", not "since session open", not "resets". Greps for
//! `cumulative`, `since`, `running total`, `reset` and `monotonic` across the
//! whole pack return ZERO hits. The neighbouring fields ARE qualified ("Last
//! Traded Quantity", "Total Sell Quantity"), which is what makes an unqualified
//! "Volume" read as day-cumulative by convention — convention, not
//! documentation. `ErrorCode::Volume01MonotonicityBreach`'s own doc cites Dhan
//! Ticket #5525125 as proof of the semantic; that ticket is about PrevClose
//! routing and carries no volume fact. The gate is built to be correct under
//! EITHER reading, which is why it is worth having while the question is open.
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
//! **The two post-sort passes DO allocate**, and neither is per tick:
//! [`distinct_underlying_over`] takes two `Vec`s of `k` (the depth-200 path,
//! `k = 5`), and [`gainer_eligible`] takes one `Vec` of `limit` plus a
//! per-underlying verdict memo (the depth-20 path, `limit = 300`). Both run
//! once per 5-second sweep on the drain's timer arm. Stated because an earlier
//! version of this header said "allocation happens once at construction"
//! without qualification, which was true of `rank` and false of these paths,
//! and a later version named a `rank_distinct_underlying` method that was
//! deleted on 2026-09-08 (its only callers were its own tests — the production
//! depth-200 publish had used `distinct_underlying_over` since the day it was
//! wired).

use std::collections::HashMap;

use tickvault_common::error_code::ErrorCode;
use tickvault_common::types::ExchangeSegment;
use tickvault_storage::top_volume_rank_persistence::SnapshotCadence;
use tracing::{error, warn};

/// Contracts tracked before the map refuses new ones.
///
/// 25,000 deliberately, matching `AGGREGATOR_MAX_SLOTS`,
/// `MAX_INDICATOR_INSTRUMENTS` and the two per-instrument tracker caps, so one
/// figure covers every per-instrument structure on the same universe.
///
/// **CORRECTED 2026-09-06 — this is PER FAMILY, not per leaderboard.** There
/// are two `Family` maps, so the structure's real ceiling is 50,000 entries and
/// its memory bound is twice what a reader would take from the number. The
/// previous doc said "the authorized set measured 22,996 on 2026-08-21, leaving
/// 2,004 spare", which compares a per-family cap against the WHOLE subscribed
/// universe — two different denominators.
///
/// The honest per-family figures from that same measurement: **20,220 stock
/// options** and **1,250 index options**. So the stock family runs at ~81% of
/// its cap and the index family at ~5%, and `RefusedAtCapacity` is not reachable
/// on either at today's scope. It is kept as a fail-closed bound rather than
/// deleted, because the ATM window is re-fitted per session and a wider one
/// would move the stock figure.
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
    ///
    /// This is the OBSERVATION — what the vendor's counter reads. It is what
    /// the monotonicity gate guards and what the baselines below are measured
    /// against. Since 2026-09-07 it is no longer the sort key.
    pub volume: u32,
    /// The RANK KEY: lots traded in the window that just closed, × 1000.
    ///
    /// **Written by [`VolumeLeaderboard::rank`], ignored by
    /// [`VolumeLeaderboard::observe`].** A caller constructing an observation
    /// leaves it 0 and the ranking overwrites it; a value set here on the way
    /// IN has no effect and is not read.
    ///
    /// Milli-lots rather than whole lots because a 250-deep board is decided at
    /// its bottom edge: with integer lots every contract trading under one lot
    /// in a one-second window collapses to 0 and the cut becomes an arbitrary
    /// tie-break rather than a measurement. See the 2026-09-07 scope-lock
    /// section.
    pub window_lots_milli: u64,
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
    /// The stored high was ABANDONED after
    /// [`RELATCH_AFTER_CONSECUTIVE_LOWER`] consecutive lower observations, and
    /// the offered volume adopted in its place.
    ///
    /// This is the escape from the one-way ratchet. Without it a contract
    /// latched to a garbage high refuses every later value for the rest of the
    /// session and holds a depth slot it does not deserve — the same "a value
    /// nothing can ever beat" failure that killed the min-heap design, one
    /// level down.
    Relatched {
        /// The high we stopped defending.
        abandoned: u32,
        /// The value taken in its place.
        adopted: u32,
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

/// Consecutive LOWER observations after which the stored high is abandoned.
///
/// Without this the gate is a one-way ratchet: a contract latched to a garbage
/// high refuses every later value for the rest of the session and holds its
/// depth slot forever. That is precisely the failure the module header cites as
/// the reason the min-heap design was killed — "a value nothing can ever beat"
/// — reproduced one level down, per contract instead of per set. An earlier
/// version of that header claimed the design "self-corrects the moment a good
/// value arrives"; it could not, and this constant is what makes the claim
/// true.
///
/// The two causes of a falling volume that are REAL restarts — a 32-bit wrap
/// and the session reset — produce an UNBOUNDED run of lower values: every
/// subsequent tick is lower. The one cause that is transient — write-ahead-log
/// replay re-injecting older frames — is bounded by the replay batch. Counting
/// consecutive refusals separates them without a clock, which the per-tick path
/// cannot afford.
///
/// 32 rather than a smaller number because a replay re-injects a contract's
/// frames in order, so each replayed frame is higher than the last while still
/// below the stored high — the counter climbs through the replay. Re-latching
/// mid-replay costs a temporarily low ranking that the next live tick corrects.
/// Re-latching too LATE costs a permanently frozen socket. The asymmetry is why
/// this errs toward re-latching.
pub const RELATCH_AFTER_CONSECUTIVE_LOWER: u16 = 32;

/// One tracked contract plus the state the re-latch needs.
///
/// `Copy`. Its payload is the 40-byte contract, the 2-byte run counter and
/// one `u32` baseline per cadence, so with padding it is ~56 bytes; the map
/// entry adds the 16-byte composite key and the hash table's own overhead,
/// so at the 25,000-per-family cap the structure is ~1.5–2 MB per family,
/// ~4 MB in all. (An earlier version of this comment said "8 bytes wider than
/// the contract, ~200 KB per family", which counted neither the baselines
/// nor the map.) It buys the difference between a transient mis-ranking and
/// a socket frozen for the session.
#[derive(Debug, Clone, Copy)]
struct Tracked {
    contract: RankedContract,
    /// Reset to 0 on every accepted advance. Only a RUN of lower values counts.
    consecutive_lower: u16,
    /// Cumulative volume as of this contract's last snapshot, PER CADENCE.
    ///
    /// One entry per window because the 1s and 5s boards measure different
    /// intervals: sharing a baseline would make whichever cadence fired last
    /// steal the other's window, and both boards would report a figure neither
    /// interval actually traded.
    ///
    /// Seeded to the contract's CURRENT volume when it is first tracked, never
    /// to 0. A 0 seed makes the first window report the whole day so far, which
    /// is the largest number that contract will ever show and would hand it a
    /// depth socket on its first appearance — the "stale high" shape this
    /// module already refuses one level up.
    baseline: [u32; WINDOW_COUNT],
}

/// Distinct snapshot cadences, and therefore baselines per contract.
///
/// Derived from the cadence enum rather than written as a literal: adding a
/// third cadence must fail to compile here rather than silently index out of
/// range or, worse, alias two cadences onto one baseline.
pub const WINDOW_COUNT: usize = SnapshotCadence::ALL.len();

/// Fixed-point scale on the rank key. 1000 = three decimal places of a lot.
pub const LOTS_SCALE: u64 = 1_000;

const _: () = assert!(
    (u32::MAX as u64).checked_mul(LOTS_SCALE).is_some(),
    "the rank key multiplies a u32 delta by LOTS_SCALE in u64; if that can \
     overflow, a busy contract's key wraps to a small number and it silently \
     leaves the board"
);

/// Which baseline slot a cadence owns.
const fn window_index(cadence: SnapshotCadence) -> usize {
    match cadence {
        SnapshotCadence::OneSecond => 0,
        SnapshotCadence::FiveSecond => 1,
    }
}

/// Lots traded in a window, × [`LOTS_SCALE`], from raw units and a lot size.
///
/// The operator's own arithmetic (2026-09-07): 20,000 units on a 200-unit lot
/// is 100 lots. Integer throughout — a float key would make the comparator
/// non-transitive the moment a NaN reached it, which corrupts a sort wholesale
/// rather than misplacing one row.
///
/// Returns `None` for a zero lot size rather than defaulting it. That cannot
/// happen through the production path (`LegRefusal::MissingLotSize` refuses it
/// at the join) and is refused again here because this is the function that
/// would otherwise divide by it.
#[must_use]
pub const fn window_lots_milli(delta_units: u32, lot_size: u32) -> Option<u64> {
    if lot_size == 0 {
        return None;
    }
    Some((delta_units as u64 * LOTS_SCALE) / lot_size as u64)
}

#[derive(Debug)]
struct Family {
    volumes: HashMap<ContractKey, Tracked>,
    non_monotonic: u64,
    at_capacity: u64,
    relatched: u64,
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
    /// Gauge handles, resolved for the SAME reason as the counters above.
    ///
    /// These sit on the 5-second cadence rather than the per-tick path, so the
    /// allocating arm would have cost three `Vec`s per sweep — real but small.
    /// Resolved anyway because a module whose doc explains this exact mechanism
    /// and then does it is a file the next reader stops trusting.
    tracked_gauge: metrics::Gauge,
    ranked_gauge: metrics::Gauge,
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
        let tracked_gauge = metrics::gauge!(TRACKED_GAUGE, "family" => label);
        let ranked_gauge = metrics::gauge!(RANKED_GAUGE, "family" => label);
        Self {
            // Pre-sized: an unsized map reallocates and rehashes ~15 times on
            // its way to the authorized universe, and every one of those lands
            // on the per-tick path.
            volumes: HashMap::with_capacity(MAX_TRACKED_CONTRACTS),
            non_monotonic: 0,
            at_capacity: 0,
            relatched: 0,
            refused_non_monotonic,
            refused_capacity,
            tracked_gauge,
            ranked_gauge,
        }
    }

    /// Clears the tracked contracts and counters, KEEPING the map capacity and
    /// the resolved handles. Replacing the struct would hand back the capacity
    /// and pay the growth again every session.
    fn clear(&mut self) {
        self.volumes.clear();
        self.non_monotonic = 0;
        self.at_capacity = 0;
        self.relatched = 0;
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
            if contract.volume < existing.contract.volume {
                let stored = existing.contract.volume;
                existing.consecutive_lower = existing.consecutive_lower.saturating_add(1);
                let run = existing.consecutive_lower;

                // RE-LATCH. A run this long is not a replayed frame; it is the
                // contract's real volume, and the stored high is garbage we
                // would otherwise defend for the rest of the session.
                //
                // Refusing forever is what made the ONE-WAY RATCHET: a contract
                // latched near `u32::MAX` outranks everything real and holds a
                // depth socket until the process restarts. That is the exact
                // failure this module's header cites as the reason the min-heap
                // design was killed — "a value nothing can ever beat" — and
                // refusing without a way back reproduces it per contract.
                //
                // Adopting the new value costs a temporarily low ranking that
                // the next genuine tick corrects. The asymmetry decides it.
                if run >= RELATCH_AFTER_CONSECUTIVE_LOWER {
                    *existing = Tracked {
                        contract,
                        consecutive_lower: 0,
                        // RESEEDED, not preserved. A re-latch is the decision
                        // that the stored series was garbage, and the baseline
                        // was measured against that same garbage — keeping it
                        // would hold this contract's window at 0 until it
                        // climbed back past a number we have just declared
                        // wrong. Its first window after a re-latch reports
                        // nothing, which is the honest answer.
                        baseline: [contract.volume; WINDOW_COUNT],
                    };
                    slot.relatched = slot.relatched.saturating_add(1);
                    let relatched_total = slot.relatched;
                    warn!(
                        metric = REFUSED_COUNTER,
                        family = label,
                        security_id = contract.security_id,
                        ?contract.segment,
                        abandoned_high = stored,
                        adopted = contract.volume,
                        consecutive_lower = run,
                        relatched_total,
                        "abandoning a stored volume high after a sustained run of lower \
                         observations — treating it as a real restart (32-bit wrap or a \
                         missed session reset) rather than a replayed frame. Refusing \
                         forever would hold this contract's depth slot for the session."
                    );
                    return Observation::Relatched {
                        abandoned: stored,
                        adopted: contract.volume,
                    };
                }

                slot.non_monotonic = slot.non_monotonic.saturating_add(1);
                let seen = slot.non_monotonic;
                slot.refused_non_monotonic.increment(1);
                // Powers of two, so a storm cannot flood the sink. A wrapped
                // contract refuses on every subsequent tick for the rest of the
                // session, which is thousands of events for one condition.
                if seen.is_power_of_two() {
                    error!(
                        code = ErrorCode::Volume01MonotonicityBreach.code_str(),
                        // The counter NAME belongs in the line, not just the handle. The
                        // handle above is what keeps the per-tick path allocation-free, but
                        // it moved the literal into `Family::new`, so an operator reading
                        // errors.jsonl had no way to know WHICH series records this, and
                        // `loss_counter_visibility_guard` could no longer see that this
                        // emit site is logged at all. Naming it here fixes both.
                        metric = REFUSED_COUNTER,
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
            if contract.volume == existing.contract.volume {
                // Nothing to record and nothing wrong. Deliberately NOT counted
                // and NOT logged. The run is deliberately NOT reset either: an
                // equal value is not evidence the stored high is good, and
                // resetting here would let a wrapped contract that alternates
                // equal/lower never reach the re-latch.
                return Observation::Unchanged;
            }
            // Advance in place. The underlying is refreshed too: a derivative
            // id can be reused across days, and holding a stale underlying
            // would put the contract under the wrong name in the depth-200
            // distinct-underlying constraint.
            *existing = Tracked {
                contract,
                consecutive_lower: 0,
                // PRESERVED, deliberately. This is a normal advance inside a
                // window that has not closed yet; reseeding here would zero
                // the delta on every accepted tick and every window would
                // report 0, which is the whole feature silently doing nothing.
                baseline: existing.baseline,
            };
            return Observation::Accepted;
        }

        if slot.volumes.len() >= MAX_TRACKED_CONTRACTS {
            slot.at_capacity = slot.at_capacity.saturating_add(1);
            let seen = slot.at_capacity;
            slot.refused_capacity.increment(1);
            if seen.is_power_of_two() {
                error!(
                    code = ErrorCode::Volume01MonotonicityBreach.code_str(),
                    // The counter NAME belongs in the line, not just the handle. The
                    // handle above is what keeps the per-tick path allocation-free, but
                    // it moved the literal into `Family::new`, so an operator reading
                    // errors.jsonl had no way to know WHICH series records this, and
                    // `loss_counter_visibility_guard` could no longer see that this
                    // emit site is logged at all. Naming it here fixes both.
                    metric = REFUSED_COUNTER,
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

        slot.volumes.insert(
            key,
            Tracked {
                contract,
                consecutive_lower: 0,
                // Seeded to what this contract has ALREADY traded today, never
                // to 0. Seeding 0 would make its first window report the whole
                // session so far — the largest figure it will ever show, on a
                // board whose top entries take depth sockets.
                baseline: [contract.volume; WINDOW_COUNT],
            },
        );
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
    ///
    /// # `k` and what the ranked gauge then means
    ///
    /// The production 5-second arm calls this with `k = usize::MAX` (clamped
    /// by `truncate`) so that ONE sort yields the FULL traded population,
    /// which the gainer filter and the distinct-underlying pass then narrow
    /// without ranking a second time — a second `rank` on the same cadence
    /// inside one tick would measure an already-consumed window. So
    /// `tv_volume_leaderboard_ranked` reports the number of contracts that
    /// TRADED in the window (the sorted population), not the 250 or 5 the
    /// depth pools were handed; the handed counts are on the steering side.
    /// The sort is O(n log n) in that traded population — MEASURED at 900 µs
    /// for the 20,220-contract worst case by
    /// `rank_sweep_cost_at_the_authorized_ceiling`.
    pub fn rank<F, L>(
        &mut self,
        family: OptionFamily,
        cadence: SnapshotCadence,
        k: usize,
        lot_of: L,
        eligible: F,
    ) -> &[RankedContract]
    where
        F: Fn(&RankedContract) -> bool,
        L: Fn(&RankedContract) -> Option<u32>,
    {
        let idx = window_index(cadence);
        // `take` rather than a direct fill: the buffer and the family map both
        // live on `self`, so filling one from the other needs two borrows. Take
        // moves the Vec out WITH its capacity, so the allocation is still made
        // once at construction and reused for the life of the process.
        let mut scratch = std::mem::take(&mut self.scratch);
        scratch.clear();

        // ONE pass, and it does three things that must not be split apart:
        // computes each contract's delta against ITS baseline, pushes the
        // eligible ones, and rolls EVERY baseline forward.
        //
        // "Every" is the load-bearing word. Rolling forward only the ranked
        // contracts would let an ineligible one accumulate across windows and
        // arrive with a delta measuring minutes the moment it became eligible —
        // it would take a depth socket on a number no other contract on the
        // board was measured over.
        for tracked in self.family_mut(family).volumes.values_mut() {
            let delta = tracked
                .contract
                .volume
                .saturating_sub(tracked.baseline[idx]);
            tracked.baseline[idx] = tracked.contract.volume;

            let mut row = tracked.contract;
            // A missing lot size cannot reach here through production — the
            // join refuses it — so this is the defensive arm, and it SKIPS
            // rather than ranking the contract at 0, which would put it in an
            // arbitrary tie at the bottom of the board instead of out of it.
            let Some(lots) = lot_of(&row).and_then(|lot| window_lots_milli(delta, lot)) else {
                continue;
            };
            // A contract that traded NOTHING in the window is not "top volume"
            // and is left OFF the board — the same treatment as a missing lot
            // size, for the same reason. Under the window key a zero is the
            // COMMON value (every quiet contract, every first sweep after a
            // boot or restart, every thin 1-second window), so ranking zeros
            // would pad the tail of a 250-deep board with contracts ordered by
            // nothing but their security_id, and the depth pools would swap
            // sockets onto strikes that traded nothing. Found by the
            // 2026-09-08 adversarial sweep; before it, the first sweep after
            // every boot published up to 250 zero-lot "top" contracts.
            if lots == 0 {
                continue;
            }
            row.window_lots_milli = lots;
            if eligible(&row) {
                scratch.push(row);
            }
        }

        // Most lots traded in the window first, then a DETERMINISTIC tiebreak
        // on the composite identity. Without the tiebreak, equal keys order by
        // whatever the hash map yielded, so the same input reorders between
        // sweeps and the caller swaps subscriptions for nothing — and equal
        // keys are COMMON now in a way they were not under a cumulative key:
        // every contract that traded nothing in the window is exactly 0.
        scratch.sort_unstable_by(|a, b| {
            b.window_lots_milli
                .cmp(&a.window_lots_milli)
                .then_with(|| a.security_id.cmp(&b.security_id))
                .then_with(|| (a.segment as u8).cmp(&(b.segment as u8)))
        });
        scratch.truncate(k);
        self.scratch = scratch;

        let ranked_len = self.scratch.len();
        let slot = self.family_ref(family);
        slot.tracked_gauge.set(slot.volumes.len() as f64);
        slot.ranked_gauge.set(ranked_len as f64);
        &self.scratch
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

    /// Stored highs ABANDONED after a sustained run of lower observations.
    ///
    /// Deliberately NOT a new metric name. A re-latch is the RESOLUTION of a
    /// run of refusals that `tv_volume_leaderboard_refused_total` has already
    /// counted, and the event itself carries a `warn!` naming the contract, the
    /// abandoned high and the adopted value. Adding a second series would cost
    /// ~$0.30/mo against a September forecast of $142.24 with the automatic
    /// `STOP_EC2_INSTANCES` line at $135.00, to report something the log
    /// already says.
    #[must_use]
    pub const fn relatches(&self, family: OptionFamily) -> u64 {
        self.family_ref(family).relatched
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

/// Greedy "at most one contract per underlying" pass over an already-ordered
/// ranking.
///
/// The depth-200 rule: operator, 2026-09-06, *"ensure on depth 200 top 5
/// should never ever be same symbols strike"*. Greedy over the volume order,
/// so each underlying is represented by its heaviest contract. **The honest
/// cost, which the scope lock records:** when the true top five are five
/// strikes of one stock, this forces four substitutions into thinner books —
/// the shape that produced 800 rows/minute against 100,800 on 2026-08-26.
///
/// A free function over an ALREADY-ranked slice, deliberately — the
/// production 5-second arm ranks the full population ONCE and applies this
/// to that slice. It was extracted on 2026-09-08 from a `rank_distinct_underlying`
/// method that ranked internally, and that method was deleted the same day
/// once its only callers were its own tests, because "without ranking again"
/// is the load-bearing half. [`VolumeLeaderboard::rank`] rolls each contract's per-cadence baseline
/// forward as it sweeps, so a second `rank` on the same cadence inside one
/// timer tick measures a window that has already been consumed: every delta
/// comes back 0 and the order collapses to the `security_id` tie-break. That is
/// a confident, completely wrong ranking with no counter to show for it, which
/// is the class this repository keeps removing.
///
/// `ordered` must already be sorted best-first; this function does not sort and
/// makes no attempt to check, because the only honest check is the sort itself.
///
/// O(k × distinct-seen) with both bounded by `k`. `k` is 5 on the depth-200
/// path, so the linear `contains` is cheaper than a set.
#[must_use]
pub fn distinct_underlying_over(ordered: &[RankedContract], k: usize) -> Vec<RankedContract> {
    // CLAMPED before the allocations. `Vec::with_capacity(k)` PANICS on a
    // capacity overflow and the release profile is `panic = "abort"`, so an
    // unclamped caller-supplied `k` is a process abort rather than a bad
    // ranking.
    let k = k.min(MAX_TRACKED_CONTRACTS);
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
    out
}

// ---------------------------------------------------------------------------
// Gainer eligibility (2026-09-06 lock: gainer = filter, volume = order)
// ---------------------------------------------------------------------------

/// The segment a STOCK option's underlying trades in.
///
/// The [`OptionFamily::Stock`] board ranks `NSE_FNO` contracts whose
/// underlying is an NSE cash equity — the same `(underlying, NSE_EQ)` key the
/// spot store and the previous-close store hold. Index options are never on
/// the depth path (the lock bans them), so this is the only underlying
/// segment the eligibility filter needs, and naming it once keeps the two
/// lookups below from ever disagreeing about which store row is "the
/// underlying".
pub const STOCK_OPTION_UNDERLYING_SEGMENT: ExchangeSegment = ExchangeSegment::NseEquity;

/// Metric: how the gainer filter judged each ranked contract's underlying,
/// per 5-second pass. Labels are [`GAINER_VERDICT_LABELS`]. Local exporter
/// only — never EMF-shipped (three series a human reads while already
/// looking at the console; the cardinality rule in the noise lock §2.3).
pub const GAINER_FILTER_COUNTER: &str = "tv_depth200_gainer_filter_total";

/// Every label value [`GAINER_FILTER_COUNTER`] can carry, for pre-registration
/// (the CloudWatch agent drops the first sample of an unseen series).
pub const GAINER_VERDICT_LABELS: [&str; 3] = ["gainer", "not_gainer", "unknown"];

/// Whether an underlying counts as a gainer for depth eligibility.
///
/// The 2026-09-06 lock: *"An instrument qualifies if its underlying is in the
/// day's gainers; volume then decides the order."* A contract on a FALLING
/// stock is not a candidate however busy it is; a contract whose underlying
/// cannot be judged is not a candidate either, and is counted separately so
/// a day where nothing can be judged (no previous closes arrived) reads as
/// `unknown`, never as "no gainers".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GainerVerdict {
    /// Spot is strictly above the previous close.
    Gainer,
    /// Spot is at or below the previous close.
    NotGainer,
    /// Either input is absent or not a price (no spot today, no previous
    /// close, non-finite, non-positive).
    Unknown,
}

impl GainerVerdict {
    /// The metric label for this verdict.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Gainer => "gainer",
            Self::NotGainer => "not_gainer",
            Self::Unknown => "unknown",
        }
    }
}

/// Judges one underlying from the RAM spot price (paise, as the spot store
/// holds it) and the previous close (rupees, as the prev-close store holds
/// it).
///
/// The comparison is done in INTEGER paise on both sides, through the same
/// `rupees_to_paise` the ladder-centring path uses — so a previous close that
/// cannot centre a ladder cannot make a gainer either, and no float ever
/// enters the decision. Zero and negative previous closes are the documented
/// sentinels and land in `Unknown`, never in `NotGainer`.
#[must_use]
pub fn underlying_gainer_verdict(
    spot_paise: Option<i64>,
    prev_close: Option<f64>,
) -> GainerVerdict {
    let (Some(spot), Some(close_paise)) = (
        spot_paise,
        prev_close.and_then(crate::spot_price_store::rupees_to_paise),
    ) else {
        return GainerVerdict::Unknown;
    };
    if spot <= 0 {
        return GainerVerdict::Unknown;
    }
    if spot > close_paise {
        GainerVerdict::Gainer
    } else {
        GainerVerdict::NotGainer
    }
}

/// How many underlyings fell into each verdict during one filter pass.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GainerTally {
    /// Contracts whose underlying is a gainer — the eligible set.
    pub gainer: usize,
    /// Contracts whose underlying is flat or falling.
    pub not_gainer: usize,
    /// Contracts whose underlying could not be judged.
    pub unknown: usize,
}

/// Keeps the first `limit` ranked contracts whose underlying `verdict_of` calls a
/// gainer, PRESERVING the volume order. Pure.
///
/// `verdict_of` is called once per DISTINCT underlying visited (memoised —
/// the ~102 strikes of one stock share a verdict), so the caller pays one
/// spot-store probe and one prev-close probe per underlying, and the walk
/// STOPS once `limit` gainers are collected — so on an ordinary day it visits
/// a few hundred rows of a population that may be thousands. The tally counts
/// rows visited, not probes made. **The honest worst case, which is a real
/// day and not a corner:** every stock falling, so nothing is ever collected
/// and the walk runs to the END of the ranked population — O(n) in the traded
/// population with one hash probe per row, MEASURED by
/// `gainer_eligible_sweep_cost_at_the_authorized_ceiling`. Cold: once per
/// 5-second sweep on the drain's timer arm, never per tick. (Until 2026-09-08
/// the probes were per ROW, so that same down day cost ~40,000 store probes
/// per sweep; the memo caps them at the underlying count whatever the day.)
///
/// Applied AFTER the volume sort rather than inside `rank` deliberately: the
/// persisted `top_volume_rank` rows must keep recording which contracts were
/// busiest whether or not their stock rose, so the filter narrows only what
/// the DEPTH steering is handed, never the record.
#[must_use]
pub fn gainer_eligible<F>(
    ranked: &[RankedContract],
    limit: usize,
    verdict_of: F,
) -> (Vec<RankedContract>, GainerTally)
where
    F: Fn(u64) -> GainerVerdict,
{
    let mut out: Vec<RankedContract> = Vec::with_capacity(ranked.len().min(limit));
    let mut tally = GainerTally::default();
    // One verdict per UNDERLYING, not per row. A stock's ~102 strikes share
    // one spot and one previous close, so the verdict is identical for every
    // one of them; without the memo a down day — every underlying falling,
    // nothing collected, the walk running to the END of the population —
    // paid two store probes per row across all ~20,000. The memo caps the
    // probes at the number of underlyings (~210) whatever the day looks like.
    // Sized for the measured F&O underlying count; a larger day grows it
    // once. Cold path: one 5-second sweep, never per tick.
    let mut memo: HashMap<u64, GainerVerdict> = HashMap::with_capacity(GAINER_MEMO_CAPACITY);
    for row in ranked {
        if out.len() >= limit {
            break;
        }
        let verdict = *memo
            .entry(row.underlying_id)
            .or_insert_with(|| verdict_of(row.underlying_id));
        match verdict {
            GainerVerdict::Gainer => {
                tally.gainer = tally.gainer.saturating_add(1);
                out.push(*row);
            }
            GainerVerdict::NotGainer => tally.not_gainer = tally.not_gainer.saturating_add(1),
            GainerVerdict::Unknown => tally.unknown = tally.unknown.saturating_add(1),
        }
    }
    (out, tally)
}

/// Pre-size for the per-underlying verdict memo in [`gainer_eligible`]:
/// the 2026-08-21 measured F&O underlying count (208) rounded up. A larger
/// population grows the map once per call, on the cold sweep, and is not a
/// refusal.
pub const GAINER_MEMO_CAPACITY: usize = 256;

/// Publishes one pass's tally onto [`GAINER_FILTER_COUNTER`].
pub fn record_gainer_tally(tally: GainerTally) {
    for (label, n) in [
        ("gainer", tally.gainer),
        ("not_gainer", tally.not_gainer),
        ("unknown", tally.unknown),
    ] {
        metrics::counter!(GAINER_FILTER_COUNTER, "verdict" => label)
            .increment(u64::try_from(n).unwrap_or(u64::MAX));
    }
}

/// Seeds every verdict series at zero so its first real sample is not the
/// one the agent drops.
pub fn pre_register_gainer_filter_counter() {
    for label in GAINER_VERDICT_LABELS {
        metrics::counter!(GAINER_FILTER_COUNTER, "verdict" => label).increment(0);
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
            window_lots_milli: 0,
        }
    }

    /// Observe a contract as TRADING inside the window.
    ///
    /// The 2026-09-07 lock seeds a newly tracked contract's baseline to its
    /// current cumulative, so its first delta is 0 and it ranks nothing until
    /// it trades — tests that want a contract ON the board therefore seed it
    /// at 1 first (an untracked key only) and then observe the real volume, so
    /// `volume - 1` units are lots-in-window and the ordering the test asserts
    /// is the ordering of the volumes it names.
    fn trade(
        lb: &mut VolumeLeaderboard,
        contract: RankedContract,
        family: OptionFamily,
    ) -> Observation {
        let key = (contract.security_id, contract.segment);
        if contract.volume > 1 && !lb.family_mut(family).volumes.contains_key(&key) {
            let seed = RankedContract {
                volume: 1,
                ..contract
            };
            let _ = lb.observe(seed, family);
        }
        lb.observe(contract, family)
    }

    fn all(_: &RankedContract) -> bool {
        true
    }

    const S1: SnapshotCadence = SnapshotCadence::OneSecond;
    const S5: SnapshotCadence = SnapshotCadence::FiveSecond;

    /// Lot size 1 for the tests that predate lot normalisation.
    ///
    /// With a lot of 1 the key is `delta * 1000`, so it is ORDER-IDENTICAL to
    /// ranking on the delta itself — which keeps every ordering assertion below
    /// meaningful while the normalisation gets its own dedicated tests.
    fn lot1(_: &RankedContract) -> Option<u32> {
        Some(1)
    }

    /// The production depth-200 shape: ONE full-population rank of the stock
    /// family on the 1-second cadence, then the greedy distinct pass over it.
    /// Mirrors `LiveIngest::snapshot_top_volume`, which is the only
    /// production caller of `distinct_underlying_over`.
    fn distinct_over_full_rank(lb: &mut VolumeLeaderboard, k: usize) -> Vec<RankedContract> {
        let ordered = lb.rank(OptionFamily::Stock, S1, usize::MAX, lot1, all);
        distinct_underlying_over(ordered, k)
    }

    #[test]
    fn monotonic_volume_updates_are_accepted_and_ranked() {
        let mut lb = VolumeLeaderboard::new();
        assert_eq!(
            trade(&mut lb, stock(1, 100, 500), OptionFamily::Stock),
            Observation::Accepted
        );
        assert_eq!(
            trade(&mut lb, stock(1, 100, 900), OptionFamily::Stock),
            Observation::Accepted
        );
        assert_eq!(
            trade(&mut lb, stock(2, 200, 700), OptionFamily::Stock),
            Observation::Accepted
        );
        let ranked = lb.rank(OptionFamily::Stock, S1, 10, lot1, all);
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
        trade(&mut lb, stock(1, 100, 4_000_000_000), OptionFamily::Stock);
        assert_eq!(
            trade(&mut lb, stock(1, 100, 12), OptionFamily::Stock),
            Observation::RefusedNonMonotonic {
                stored: 4_000_000_000,
                offered: 12
            }
        );
        let ranked = lb.rank(OptionFamily::Stock, S1, 10, lot1, all);
        assert_eq!(
            ranked[0].volume, 4_000_000_000,
            "the stored high must survive the refusal"
        );
        assert_eq!(lb.non_monotonic_refusals(OptionFamily::Stock), 1);
    }

    #[test]
    fn a_contract_latched_at_the_ceiling_recovers_instead_of_owning_a_socket_forever() {
        // THE ONE-WAY RATCHET, and the reason RELATCH_AFTER_CONSECUTIVE_LOWER
        // exists. Before it, this contract refused every later value for the
        // rest of the session and held a depth socket it did not deserve --
        // the exact "a value nothing can ever beat" failure the module header
        // cites as the reason the min-heap design was killed, reproduced per
        // contract inside the design that replaced it.
        let mut lb = VolumeLeaderboard::new();
        lb.observe(stock(1, 100, u32::MAX), OptionFamily::Stock);
        lb.observe(stock(2, 200, 5_000), OptionFamily::Stock);

        // The real contract restarts near zero and climbs, as a wrapped or
        // session-reset counter does.
        for i in 1..RELATCH_AFTER_CONSECUTIVE_LOWER {
            assert!(
                matches!(
                    lb.observe(stock(1, 100, u32::from(i)), OptionFamily::Stock),
                    Observation::RefusedNonMonotonic { .. }
                ),
                "a SHORT run must still be refused -- that is the replay case"
            );
        }
        // UPDATED 2026-09-07 with the ranking key. Under the old cumulative
        // key this asserted `security_id == 1` — the latched contract sat at
        // the TOP of the board holding a socket, which is what made the
        // one-way ratchet expensive.
        //
        // Under a per-window key it sits at the BOTTOM instead: its stored
        // volume is not advancing, so its delta is 0 while contract 2 is
        // genuinely trading. The stored high is still defended — the refusals
        // above prove that, and they are unchanged — but defending it no
        // longer BUYS the contract a socket. That is a real improvement the
        // key change brings, and it is asserted rather than assumed.
        //
        // Contract 2 has to actually TRADE for that comparison to mean
        // anything: an opening rank closes both contracts' first window, then
        // 2 advances and 1 cannot.
        let _ = lb.rank(OptionFamily::Stock, S1, 10, lot1, all);
        lb.observe(stock(2, 200, 6_000), OptionFamily::Stock);
        let ranked = lb.rank(OptionFamily::Stock, S1, 10, lot1, all);
        assert_eq!(
            ranked[0].security_id, 2,
            "a contract whose volume is frozen must not out-rank one that is trading"
        );
        assert_eq!(
            lb.non_monotonic_refusals(OptionFamily::Stock),
            u64::from(RELATCH_AFTER_CONSECUTIVE_LOWER - 1),
            "the high IS still defended -- that is what the refusals record"
        );

        // The run crosses the threshold: the high is abandoned.
        assert_eq!(
            lb.observe(
                stock(1, 100, u32::from(RELATCH_AFTER_CONSECUTIVE_LOWER)),
                OptionFamily::Stock
            ),
            Observation::Relatched {
                abandoned: u32::MAX,
                adopted: u32::from(RELATCH_AFTER_CONSECUTIVE_LOWER),
            }
        );
        assert_eq!(lb.relatches(OptionFamily::Stock), 1);

        // The re-latch reseeded contract 1's baseline to the value it adopted,
        // so the window that closes here measures nothing for it — correct,
        // because we have just declared the series it was measured against
        // garbage. This rank is what closes that window for both contracts.
        let _ = lb.rank(OptionFamily::Stock, S1, 10, lot1, all);

        // And the recovered contract ranks normally again from here: it now
        // trades MORE in the window than contract 2, and takes the top slot
        // on that basis rather than on a latched high.
        assert_eq!(
            lb.observe(stock(1, 100, 9_000), OptionFamily::Stock),
            Observation::Accepted
        );
        assert_eq!(
            lb.observe(stock(2, 200, 6_100), OptionFamily::Stock),
            Observation::Accepted
        );
        let ranked = lb.rank(OptionFamily::Stock, S1, 10, lot1, all);
        assert_eq!(
            ranked[0].security_id, 1,
            "8,968 traded this window must outrank 100"
        );
        assert_eq!(
            ranked[0].window_lots_milli,
            (9_000 - u64::from(RELATCH_AFTER_CONSECUTIVE_LOWER)) * LOTS_SCALE,
            "the key is what traded IN THE WINDOW, not the cumulative 9,000"
        );
    }

    #[test]
    fn an_accepted_advance_resets_the_run_so_a_replay_burst_never_accumulates() {
        // The run must count CONSECUTIVE lower values. A WAL replay interleaved
        // with live ticks would otherwise creep to the threshold over a session
        // and abandon a perfectly good high.
        let mut lb = VolumeLeaderboard::new();
        trade(&mut lb, stock(1, 100, 1_000_000), OptionFamily::Stock);
        for _ in 0..(RELATCH_AFTER_CONSECUTIVE_LOWER * 4) {
            // one replayed frame, then one live advance
            assert!(matches!(
                trade(&mut lb, stock(1, 100, 5), OptionFamily::Stock),
                Observation::RefusedNonMonotonic { .. }
            ));
            let higher = lb.rank(OptionFamily::Stock, S1, 1, lot1, all)[0].volume + 1;
            assert_eq!(
                trade(&mut lb, stock(1, 100, higher), OptionFamily::Stock),
                Observation::Accepted
            );
        }
        assert_eq!(
            lb.relatches(OptionFamily::Stock),
            0,
            "an interleaved burst must NEVER reach the threshold"
        );
    }

    #[test]
    fn an_equal_volume_does_not_reset_the_run_toward_relatch() {
        // A wrapped contract whose volume alternates equal/lower must still
        // reach the re-latch. An equal value is not evidence the stored high is
        // good, so it must not clear the run.
        let mut lb = VolumeLeaderboard::new();
        lb.observe(stock(1, 100, u32::MAX), OptionFamily::Stock);
        // 31 lower observations, each followed by one EQUAL to the stored high.
        for _ in 1..RELATCH_AFTER_CONSECUTIVE_LOWER {
            lb.observe(stock(1, 100, 7), OptionFamily::Stock);
            assert_eq!(
                lb.observe(stock(1, 100, u32::MAX), OptionFamily::Stock),
                Observation::Unchanged,
                "a value identical to the stored high is Unchanged, never Accepted"
            );
        }
        assert_eq!(
            lb.relatches(OptionFamily::Stock),
            0,
            "31 lower values is still short of the threshold"
        );
        // The 32nd crosses it, DESPITE 31 intervening equal values.
        assert!(matches!(
            lb.observe(stock(1, 100, 7), OptionFamily::Stock),
            Observation::Relatched { .. }
        ));
        assert_eq!(
            lb.relatches(OptionFamily::Stock),
            1,
            "equal values must not shield a garbage high from the re-latch"
        );
    }

    #[test]
    fn a_relatch_refreshes_the_underlying_so_depth_200_groups_it_correctly() {
        // A derivative security_id can be reused across days. Holding the old
        // underlying would file the contract under the wrong name in the
        // distinct-underlying constraint, silently.
        let mut lb = VolumeLeaderboard::new();
        trade(&mut lb, stock(1, 100, u32::MAX), OptionFamily::Stock);
        for i in 1..=RELATCH_AFTER_CONSECUTIVE_LOWER {
            trade(&mut lb, stock(1, 999, u32::from(i)), OptionFamily::Stock);
        }
        // The re-latch re-seeds the window baseline (the contract is treated as
        // newly tracked), so it ranks only once it trades again — and the
        // stored high of u32::MAX would still refuse this value had the
        // re-latch not happened.
        trade(
            &mut lb,
            stock(1, 999, u32::from(RELATCH_AFTER_CONSECUTIVE_LOWER) + 1),
            OptionFamily::Stock,
        );
        assert_eq!(
            lb.rank(OptionFamily::Stock, S1, 1, lot1, all)[0].underlying_id,
            999,
            "the re-latch must adopt the whole contract, not just its volume"
        );
    }

    #[test]
    fn a_wrapped_counter_is_refused_by_the_same_gate_that_catches_replay() {
        // ParsedTick.volume is u32 and the WIRE value wraps at the vendor. We
        // never see the wrap; we see the fall. One gate, four causes: a wrap, a
        // WAL replay of an older frame, a vendor consumer-skip, and the 09:00
        // reset. This test is the wrap; the one above is the general case.
        let mut lb = VolumeLeaderboard::new();
        trade(&mut lb, stock(1, 100, u32::MAX - 5), OptionFamily::Stock);
        // u32::MAX - 5 plus 10 wraps to 4 on the wire.
        let wrapped = (u32::MAX - 5).wrapping_add(10);
        assert_eq!(wrapped, 4, "sanity: this is what a wrap looks like");
        assert!(matches!(
            trade(&mut lb, stock(1, 100, wrapped), OptionFamily::Stock),
            Observation::RefusedNonMonotonic { .. }
        ));
        assert_eq!(
            lb.rank(OptionFamily::Stock, S1, 1, lot1, all)[0].volume,
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
        trade(&mut lb, stock(1, 100, 500), OptionFamily::Stock);

        for _ in 0..1_000 {
            assert_eq!(
                trade(&mut lb, stock(1, 100, 500), OptionFamily::Stock),
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
            trade(&mut lb, stock(1, 100, 499), OptionFamily::Stock),
            Observation::RefusedNonMonotonic { .. }
        ));
        assert_eq!(lb.non_monotonic_refusals(OptionFamily::Stock), 1);
        assert_eq!(
            lb.rank(OptionFamily::Stock, S1, 1, lot1, all)[0].volume,
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
            lb.rank(OptionFamily::Stock, S1, 250, lot1, all).is_empty(),
            "an all-zero field must rank nothing, never an arbitrary 250"
        );
    }

    #[test]
    fn index_and_stock_options_are_ranked_in_separate_leaderboards() {
        let mut lb = VolumeLeaderboard::new();
        trade(&mut lb, stock(1, 100, 900), OptionFamily::Stock);
        trade(&mut lb, stock(2, 200, 5_000_000), OptionFamily::Index);
        assert_eq!(lb.tracked(OptionFamily::Stock), 1);
        assert_eq!(lb.tracked(OptionFamily::Index), 1);
        let stock_ranked = lb.rank(OptionFamily::Stock, S1, 10, lot1, all).to_vec();
        assert_eq!(stock_ranked.len(), 1);
        assert_eq!(stock_ranked[0].security_id, 1);
        assert_eq!(
            lb.rank(OptionFamily::Index, S1, 10, lot1, all)[0].security_id,
            2
        );
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
            trade(&mut lb, c, OptionFamily::Index);
            blended.push(c);
        }
        for id in 1_000..1_300u64 {
            let c = stock(id, id % 40, 5_000 + id as u32);
            trade(&mut lb, c, OptionFamily::Stock);
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
            lb.rank(OptionFamily::Stock, S1, 250, lot1, all).len(),
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
        // subscriptions for nothing. Under the window key every contract must
        // TRADE the same amount in each window to tie — a zero window is off
        // the board, not a tie at the bottom of it.
        let mut lb = VolumeLeaderboard::new();
        for id in (0..40u64).rev() {
            lb.observe(stock(id, id, 777), OptionFamily::Stock);
        }
        let _seed = lb.rank(OptionFamily::Stock, S1, 40, lot1, all);
        for id in (0..40u64).rev() {
            lb.observe(stock(id, id, 1_777), OptionFamily::Stock);
        }
        let first: Vec<u64> = lb
            .rank(OptionFamily::Stock, S1, 40, lot1, all)
            .iter()
            .map(|c| c.security_id)
            .collect();
        for id in (0..40u64).rev() {
            lb.observe(stock(id, id, 2_777), OptionFamily::Stock);
        }
        let second: Vec<u64> = lb
            .rank(OptionFamily::Stock, S1, 40, lot1, all)
            .iter()
            .map(|c| c.security_id)
            .collect();
        assert_eq!(first, second, "two sweeps of identical deltas must agree");
        assert_eq!(first.len(), 40);
        assert_eq!(first[0], 0, "the tiebreak is ascending on security_id");
        assert_eq!(first[39], 39);
    }

    #[test]
    fn eligibility_filters_membership_while_volume_decides_order() {
        let mut lb = VolumeLeaderboard::new();
        trade(&mut lb, stock(1, 100, 900), OptionFamily::Stock);
        trade(&mut lb, stock(2, 200, 800), OptionFamily::Stock);
        trade(&mut lb, stock(3, 300, 700), OptionFamily::Stock);
        let ranked = lb.rank(OptionFamily::Stock, S1, 10, lot1, |c: &RankedContract| {
            c.underlying_id != 100
        });
        assert_eq!(ranked.len(), 2, "underlying 100 is filtered out");
        assert_eq!(ranked[0].security_id, 2, "volume still decides the order");
        assert_eq!(ranked[1].security_id, 3);
    }

    #[test]
    fn distinct_underlying_over_clamps_k_rather_than_aborting_the_process() {
        // The pure pass, tested directly. The production depth-200 candidate
        // publish applies it to an already-ranked slice (calling `rank` twice
        // in one tick consumes the window and silently collapses the order),
        // so a defect here reaches the five deep sockets directly.
        let row = |sid: u64, underlying: u64| RankedContract {
            security_id: sid,
            segment: ExchangeSegment::NseFno,
            underlying_id: underlying,
            volume: 0,
            window_lots_milli: 0,
        };

        // Distinctness: five strikes of one name yield ONE entry.
        let one_name: Vec<RankedContract> = (0..5).map(|s| row(s, 42)).collect();
        let picked = distinct_underlying_over(&one_name, 5);
        assert_eq!(
            picked.len(),
            1,
            "five strikes of one name are one candidate"
        );
        assert_eq!(picked[0].security_id, 0, "the first — i.e. heaviest — wins");

        // Order is preserved from the caller's sort; this fn never re-sorts.
        let many: Vec<RankedContract> = (0..8).map(|s| row(s, s)).collect();
        let five = distinct_underlying_over(&many, 5);
        assert_eq!(
            five.iter().map(|c| c.security_id).collect::<Vec<_>>(),
            vec![0, 1, 2, 3, 4]
        );

        // THE reason the clamp exists. `Vec::with_capacity(k)` panics on a
        // capacity overflow and the release profile is `panic = "abort"`, so
        // an unclamped k from a caller would be a process abort — the whole
        // trading lane — rather than a short list.
        let clamped = distinct_underlying_over(&many, usize::MAX);
        assert_eq!(
            clamped.len(),
            8,
            "an absurd k must clamp and return what exists, never abort"
        );

        // Honest under-fill, and the empty case.
        assert_eq!(distinct_underlying_over(&many, 0).len(), 0);
        assert_eq!(distinct_underlying_over(&[], 5).len(), 0);
    }

    #[test]
    fn distinct_over_the_full_rank_takes_the_heaviest_contract_per_name() {
        // The depth-200 rule. Five strikes of one stock become one entry, and
        // the remaining four slots go to the next four names.
        let mut lb = VolumeLeaderboard::new();
        for strike in 0..5u64 {
            trade(
                &mut lb,
                stock(strike, 100, 9_000 - strike as u32),
                OptionFamily::Stock,
            );
        }
        for name in 1..5u64 {
            trade(
                &mut lb,
                stock(100 + name, 100 + name, 1_000),
                OptionFamily::Stock,
            );
        }
        let picked = distinct_over_full_rank(&mut lb, 5);
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
    fn distinct_over_the_full_rank_returns_fewer_than_k_rather_than_repeating_a_name() {
        // Honest under-fill. Returning four when only four names exist is
        // correct; padding with a second strike of a name would break the
        // operator's rule silently.
        let mut lb = VolumeLeaderboard::new();
        for strike in 0..20u64 {
            trade(
                &mut lb,
                stock(strike, strike % 4, 5_000 + strike as u32),
                OptionFamily::Stock,
            );
        }
        let picked = distinct_over_full_rank(&mut lb, 5);
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
            trade(
                &mut lb,
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
            trade(
                &mut lb,
                stock(1_000 + name, name, volume),
                OptionFamily::Stock,
            );
        }

        let picked = distinct_over_full_rank(&mut lb, 5);

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
            trade(
                &mut lb,
                stock(id, 900 + id, u32::MAX - id as u32),
                OptionFamily::Index,
            );
        }
        trade(&mut lb, stock(50, 100, 2), OptionFamily::Stock);

        let top = lb.rank(OptionFamily::Stock, S1, 250, lot1, all);
        assert_eq!(top.len(), 1, "only the one stock contract is rankable");
        assert_eq!(top[0].security_id, 50);
        // A sweep rolls the baseline; trade again so the second sweep has a window.
        trade(&mut lb, stock(50, 100, 3), OptionFamily::Stock);

        let five = distinct_over_full_rank(&mut lb, 5);
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
        let _ = lb.rank(OptionFamily::Stock, S1, DEPTH_20, lot1, all);

        const ROUNDS: u32 = 50;
        let start = std::time::Instant::now();
        for _ in 0..ROUNDS {
            let top = lb.rank(OptionFamily::Stock, S1, DEPTH_20, lot1, all);
            std::hint::black_box(top.len());
        }
        let per_sweep = start.elapsed() / ROUNDS;

        // The depth-200 path: ONE full-population sort (k = usize::MAX) and
        // the greedy distinct pass over it — the production 5-second arm shape.
        let start5 = std::time::Instant::now();
        for _ in 0..ROUNDS {
            let five = distinct_over_full_rank(&mut lb, 5);
            std::hint::black_box(five.len());
        }
        let per_distinct = start5.elapsed() / ROUNDS;

        // 5-second cadence.
        let duty = per_sweep.as_secs_f64() / 5.0 * 100.0;
        let duty5 = per_distinct.as_secs_f64() / 5.0 * 100.0;
        println!(
            "MEASURED at {CONTRACTS} contracts:\n  \
             rank(top {DEPTH_20})        = {per_sweep:?}  -> {duty:.4}% duty at 5s\n  \
             full rank + distinct(5)    = {per_distinct:?}  -> {duty5:.4}% duty at 5s"
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
                window_lots_milli: 0,
                security_id: 13,
                segment: ExchangeSegment::NseFno,
                underlying_id: 1,
                volume: 500,
            },
            OptionFamily::Stock,
        );
        lb.observe(
            RankedContract {
                window_lots_milli: 0,
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
        // UPDATED 2026-09-07: the first rank CLOSES the opening window. Every
        // contract's baseline was seeded to what it had already traded, so
        // every delta here is 0 — which is the honest answer, because a single
        // observation says nothing about how much traded inside a window.
        let opening = lb.rank(OptionFamily::Stock, S1, 250, lot1, all);
        assert!(
            opening.iter().all(|c| c.window_lots_milli == 0),
            "a contract seen once has traded nothing measurable IN a window"
        );

        // Now give each contract a window's worth of trading, deliberately in
        // the OPPOSITE order to the cumulative one: id 0 trades the most.
        for id in 0..500u64 {
            let traded = 500 - id as u32;
            lb.observe(
                stock(id, id, 1_000 + id as u32 + traded),
                OptionFamily::Stock,
            );
        }
        let ranked = lb.rank(OptionFamily::Stock, S1, 250, lot1, all);
        assert_eq!(ranked.len(), 250);
        // The contract with the LOWEST cumulative volume is now first, which
        // is the whole point of the change: the board measures the window, not
        // the morning. Under the old key this row read 1,499.
        assert_eq!(ranked[0].security_id, 0);
        assert_eq!(ranked[0].volume, 1_500, "its cumulative is mid-pack");
        assert_eq!(
            ranked[0].window_lots_milli,
            500 * LOTS_SCALE,
            "500 units on a lot of 1 is 500 lots"
        );
        assert_eq!(
            ranked[249].window_lots_milli,
            251 * LOTS_SCALE,
            "the 250th, not the lightest"
        );
    }

    #[test]
    fn the_key_is_lots_not_units_so_a_big_lot_does_not_win_on_size_alone() {
        // The defect the operator named (2026-09-07): "based on lot quantity
        // size it should be checked". Contract 1 trades 20,000 units on a
        // 200-unit lot = 100 lots. Contract 2 trades 24,000 units — MORE units
        // — on a 1,200-unit lot = 20 lots. Ranking on units puts 2 first;
        // ranking on lots puts 1 first, which is the busier book.
        let mut lb = VolumeLeaderboard::new();
        lb.observe(stock(1, 100, 1), OptionFamily::Stock);
        lb.observe(stock(2, 200, 1), OptionFamily::Stock);
        let _ = lb.rank(OptionFamily::Stock, S1, 10, lot_of_fixture, all);

        lb.observe(stock(1, 100, 1 + 20_000), OptionFamily::Stock);
        lb.observe(stock(2, 200, 1 + 24_000), OptionFamily::Stock);
        let ranked = lb.rank(OptionFamily::Stock, S1, 10, lot_of_fixture, all);

        assert_eq!(ranked[0].security_id, 1, "100 lots beats 20 lots");
        assert_eq!(ranked[0].window_lots_milli, 100 * LOTS_SCALE);
        assert_eq!(ranked[1].window_lots_milli, 20 * LOTS_SCALE);
        // And the raw units say the opposite, which is the whole point.
        assert!(ranked[1].volume > ranked[0].volume);
    }

    #[test]
    fn the_five_second_window_is_measured_independently_of_the_one_second_one() {
        // The operator gave BOTH cadences ("every second ... and even per 5
        // second also"), and they must not share a baseline: whichever fired
        // last would steal the other's interval and both boards would report a
        // window neither one measured.
        let mut lb = VolumeLeaderboard::new();
        lb.observe(stock(1, 100, 1), OptionFamily::Stock);
        let _ = lb.rank(OptionFamily::Stock, S1, 10, lot1, all);
        let _ = lb.rank(OptionFamily::Stock, S5, 10, lot1, all);

        // Five 1-second windows of 1,000 units each.
        for step in 1..=5u32 {
            lb.observe(stock(1, 100, 1 + step * 1_000), OptionFamily::Stock);
            let ranked = lb.rank(OptionFamily::Stock, S1, 10, lot1, all);
            assert_eq!(
                ranked[0].window_lots_milli,
                1_000 * LOTS_SCALE,
                "each 1s window sees only its own 1,000"
            );
        }

        // The 5s window has not been read since before any of that, so it sees
        // the WHOLE 5,000 — not the last second's 1,000, and not zero.
        let ranked = lb.rank(OptionFamily::Stock, S5, 10, lot1, all);
        assert_eq!(ranked[0].window_lots_milli, 5_000 * LOTS_SCALE);
    }

    #[test]
    fn a_newly_tracked_contract_reports_nothing_until_it_trades_in_a_window() {
        // The "stale high" guard, and its 2026-09-08 sharpening. Seeding the
        // baseline to 0 would make a contract's FIRST window report its whole
        // day so far — the largest number it will ever show — and hand it a
        // depth socket on arrival. And a contract whose window is ZERO is not
        // "top volume" at all: it is left OFF the board rather than ranked at
        // 0, because a zero-lot tail is the arbitrary "whoever ticked first"
        // set the operator's requirement excludes.
        let mut lb = VolumeLeaderboard::new();
        lb.observe(stock(1, 100, 4_000_000), OptionFamily::Stock);
        let ranked = lb.rank(OptionFamily::Stock, S1, 10, lot1, all);
        assert!(
            ranked.is_empty(),
            "four million units traded BEFORE we were watching is not a window, \
             and a zero window is not a place on the board"
        );
        // The observation itself is kept: the next window measures from it.
        lb.observe(stock(1, 100, 4_000_050), OptionFamily::Stock);
        let ranked = lb.rank(OptionFamily::Stock, S1, 10, lot1, all);
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].window_lots_milli, 50 * LOTS_SCALE);
        assert_eq!(ranked[0].volume, 4_000_050);
    }

    #[test]
    fn a_contract_with_no_usable_lot_size_is_left_off_the_board_entirely() {
        // Skipped, never ranked at 0: a 0 key would put it in an arbitrary
        // tie at the bottom of a 250-deep board rather than out of it, and the
        // bottom edge is exactly where the cut is decided.
        let mut lb = VolumeLeaderboard::new();
        lb.observe(stock(1, 100, 1), OptionFamily::Stock);
        lb.observe(stock(2, 200, 1), OptionFamily::Stock);
        let none_for_one = |c: &RankedContract| (c.security_id != 1).then_some(50);
        let _ = lb.rank(OptionFamily::Stock, S1, 10, none_for_one, all);
        lb.observe(stock(1, 100, 5_000), OptionFamily::Stock);
        lb.observe(stock(2, 200, 5_000), OptionFamily::Stock);
        let ranked = lb.rank(OptionFamily::Stock, S1, 10, none_for_one, all);
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].security_id, 2);
    }

    #[test]
    fn window_lots_milli_refuses_a_zero_lot_and_never_divides_by_it() {
        assert_eq!(window_lots_milli(20_000, 200), Some(100 * LOTS_SCALE));
        assert_eq!(window_lots_milli(25_000, 200), Some(125 * LOTS_SCALE));
        assert_eq!(window_lots_milli(0, 200), Some(0));
        assert_eq!(window_lots_milli(1_000, 0), None, "never a divide by zero");
        // Sub-lot resolution is why the scale exists: 100 units on a 250 lot
        // is 0.4 lots, which must be distinguishable from nothing at all.
        assert_eq!(window_lots_milli(100, 250), Some(400));
        assert_ne!(window_lots_milli(100, 250), window_lots_milli(0, 250));
        // The widest possible delta must not overflow the key.
        assert_eq!(
            window_lots_milli(u32::MAX, 1),
            Some(u64::from(u32::MAX) * LOTS_SCALE)
        );
    }

    #[test]
    fn an_ineligible_contract_still_has_its_baseline_rolled_forward() {
        // Otherwise it accumulates across every window it sits out and arrives
        // with a delta measuring minutes, taking a socket on a number no other
        // contract on the board was measured over.
        let mut lb = VolumeLeaderboard::new();
        lb.observe(stock(1, 100, 1), OptionFamily::Stock);
        lb.observe(stock(2, 200, 1), OptionFamily::Stock);
        let only_two = |c: &RankedContract| c.security_id == 2;
        let _ = lb.rank(OptionFamily::Stock, S1, 10, lot1, only_two);

        // Contract 1 trades hard for three windows while ineligible.
        for step in 1..=3u32 {
            lb.observe(stock(1, 100, step * 10_000), OptionFamily::Stock);
            lb.observe(stock(2, 200, step * 10), OptionFamily::Stock);
            let _ = lb.rank(OptionFamily::Stock, S1, 10, lot1, only_two);
        }

        // Now it becomes eligible and trades a modest amount. Its key must
        // measure THAT window, not the 30,000 it traded while sitting out.
        lb.observe(stock(1, 100, 30_100), OptionFamily::Stock);
        let ranked = lb.rank(OptionFamily::Stock, S1, 10, lot1, all);
        let one = ranked.iter().find(|c| c.security_id == 1).expect("present");
        assert_eq!(one.window_lots_milli, 100 * LOTS_SCALE);
    }

    /// Lot 200 for contract 1, lot 1,200 for contract 2 — real-ish sizes that
    /// differ by 6x, which is what makes the units-vs-lots distinction bite.
    fn lot_of_fixture(c: &RankedContract) -> Option<u32> {
        Some(if c.security_id == 1 { 200 } else { 1_200 })
    }

    // ---- gainer eligibility (2026-09-06 lock) ----

    #[test]
    fn underlying_gainer_verdict_is_an_integer_paise_compare() {
        use GainerVerdict::{Gainer, NotGainer};
        assert_eq!(underlying_gainer_verdict(Some(11_000), Some(100.0)), Gainer);
        assert_eq!(
            underlying_gainer_verdict(Some(10_000), Some(100.0)),
            NotGainer,
            "flat is not a gainer"
        );
        assert_eq!(
            underlying_gainer_verdict(Some(9_999), Some(100.0)),
            NotGainer
        );
        assert_eq!(
            underlying_gainer_verdict(Some(10_001), Some(100.0)),
            Gainer,
            "one paise above the close is a gainer — the compare is exact, not a float band"
        );
    }

    #[test]
    fn underlying_gainer_verdict_refuses_to_judge_from_a_missing_or_sentinel_input() {
        use GainerVerdict::Unknown;
        assert_eq!(
            underlying_gainer_verdict(None, Some(100.0)),
            Unknown,
            "no spot today"
        );
        assert_eq!(
            underlying_gainer_verdict(Some(11_000), None),
            Unknown,
            "no previous close"
        );
        assert_eq!(
            underlying_gainer_verdict(Some(11_000), Some(0.0)),
            Unknown,
            "the zero sentinel"
        );
        assert_eq!(underlying_gainer_verdict(Some(11_000), Some(-1.0)), Unknown);
        assert_eq!(
            underlying_gainer_verdict(Some(11_000), Some(f64::NAN)),
            Unknown
        );
        assert_eq!(
            underlying_gainer_verdict(Some(11_000), Some(f64::INFINITY)),
            Unknown
        );
        assert_eq!(
            underlying_gainer_verdict(Some(0), Some(100.0)),
            Unknown,
            "a zero spot is an absence"
        );
        assert_eq!(underlying_gainer_verdict(Some(-5), Some(100.0)), Unknown);
    }

    #[test]
    fn gainer_eligible_keeps_the_volume_order_and_drops_falling_and_unknown_underlyings() {
        let ranked = [
            stock(1, 100, 900), // gainer
            stock(2, 200, 800), // falling
            stock(3, 300, 700), // unknown
            stock(4, 100, 600), // gainer, same underlying as #1
            stock(5, 400, 500), // gainer
        ];
        let (eligible, tally) =
            gainer_eligible(&ranked, usize::MAX, |underlying| match underlying {
                100 | 400 => GainerVerdict::Gainer,
                200 => GainerVerdict::NotGainer,
                _ => GainerVerdict::Unknown,
            });
        let ids: Vec<u64> = eligible.iter().map(|c| c.security_id).collect();
        assert_eq!(
            ids,
            vec![1, 4, 5],
            "membership by gain, order by the ranking"
        );
        assert_eq!(
            tally,
            GainerTally {
                gainer: 3,
                not_gainer: 1,
                unknown: 1
            }
        );
        let picked = distinct_underlying_over(&eligible, 5);
        let picked_ids: Vec<u64> = picked.iter().map(|c| c.security_id).collect();
        assert_eq!(
            picked_ids,
            vec![1, 5],
            "after the filter the distinct pass still keeps one contract per underlying"
        );
    }

    #[test]
    fn gainer_eligible_on_a_down_day_is_empty_and_the_tally_says_why() {
        let ranked = [stock(1, 100, 900), stock(2, 200, 800)];
        let (eligible, tally) = gainer_eligible(&ranked, usize::MAX, |_| GainerVerdict::NotGainer);
        assert!(
            eligible.is_empty(),
            "no gainers means no candidates — sockets hold"
        );
        assert_eq!(tally.not_gainer, 2);
        assert_eq!(
            tally.unknown, 0,
            "a down day must not read as an unjudgeable day"
        );
    }

    #[test]
    fn record_gainer_tally_accepts_every_shape_and_label_covers_every_verdict_once() {
        let labels = [
            GainerVerdict::Gainer.label(),
            GainerVerdict::NotGainer.label(),
            GainerVerdict::Unknown.label(),
        ];
        assert_eq!(labels, GAINER_VERDICT_LABELS);
        record_gainer_tally(GainerTally {
            gainer: 1,
            not_gainer: 2,
            unknown: 3,
        });
    }

    #[test]
    fn pre_register_gainer_filter_counter_seeds_every_label_without_panicking() {
        pre_register_gainer_filter_counter();
        pre_register_gainer_filter_counter();
    }

    /// The gainer walk stops once `limit` gainers are collected, and the tally
    /// counts only what was visited — so a 20,000-row population costs a few
    /// hundred probes on an ordinary day, not 40,000.
    #[test]
    fn gainer_eligible_stops_at_the_limit_and_tallies_only_what_it_visited() {
        let ranked: Vec<RankedContract> =
            (1..=10u64).map(|i| stock(i, i, 1_000 - i as u32)).collect();
        let (eligible, tally) = gainer_eligible(&ranked, 3, |u| {
            if u % 2 == 0 {
                GainerVerdict::Gainer
            } else {
                GainerVerdict::NotGainer
            }
        });
        let ids: Vec<u64> = eligible.iter().map(|c| c.security_id).collect();
        assert_eq!(
            ids,
            vec![2, 4, 6],
            "the first three gainers in volume order"
        );
        assert_eq!(tally.gainer, 3);
        assert_eq!(
            tally.not_gainer, 3,
            "rows 1, 3, 5 were visited; 7+ were not"
        );
        assert_eq!(tally.unknown, 0);
    }

    /// The memo: a stock's many strikes share one verdict, so the store is
    /// probed once per underlying, not once per row — and on a down day the
    /// walk still reaches the end of the population without a probe per row.
    #[test]
    fn gainer_eligible_probes_each_underlying_once_however_many_strikes_it_has() {
        use std::cell::Cell;
        // 5 underlyings × 40 strikes, all falling, so nothing is collected and
        // the walk visits every row.
        let ranked: Vec<RankedContract> = (0..200u64)
            .map(|i| stock(i, i % 5, 10_000 - i as u32))
            .collect();
        let probes = Cell::new(0usize);
        let (eligible, tally) = gainer_eligible(&ranked, 300, |_| {
            probes.set(probes.get() + 1);
            GainerVerdict::NotGainer
        });
        assert!(eligible.is_empty());
        assert_eq!(tally.not_gainer, 200, "every row was visited and tallied");
        assert_eq!(
            probes.get(),
            5,
            "but the store was probed once per underlying"
        );
    }

    /// MEASURES the gainer walk at the authorized ceiling on its WORST day —
    /// every underlying falling, so the walk cannot stop early. `#[ignore]`d
    /// like `rank_sweep_cost_at_the_authorized_ceiling`: a wall-clock number
    /// must never become a flaky CI gate.
    #[test]
    #[ignore = "wall-clock measurement, not a gate"]
    fn gainer_eligible_sweep_cost_at_the_authorized_ceiling() {
        const CONTRACTS: u64 = 20_220;
        const UNDERLYINGS: u64 = 210;
        let ranked: Vec<RankedContract> = (0..CONTRACTS)
            .map(|i| stock(i, i % UNDERLYINGS, 5_000_000 - i as u32))
            .collect();
        let _ = gainer_eligible(&ranked, 300, |_| GainerVerdict::NotGainer);
        const ROUNDS: u32 = 50;
        let start = std::time::Instant::now();
        for _ in 0..ROUNDS {
            let (out, tally) = gainer_eligible(&ranked, 300, |_| GainerVerdict::NotGainer);
            std::hint::black_box((out.len(), tally.not_gainer));
        }
        let per_walk = start.elapsed() / ROUNDS;
        let duty = per_walk.as_secs_f64() / 5.0 * 100.0;
        println!(
            "MEASURED gainer walk at {CONTRACTS} contracts / {UNDERLYINGS} underlyings, \
             all falling: {per_walk:?} -> {duty:.4}% duty at 5s"
        );
        assert!(
            per_walk < std::time::Duration::from_secs(1),
            "the down-day walk must not approach the 5s cadence: {per_walk:?}"
        );
    }

    /// Gainers ranked below the persisted cut still reach the depth pool: the
    /// filter runs on the full population, not on a pre-cut top-250.
    #[test]
    fn a_gainer_ranked_below_the_top_250_by_volume_still_qualifies_for_depth() {
        let mut lb = VolumeLeaderboard::new();
        for i in 1..=300u64 {
            lb.observe(stock(i, i, 1), OptionFamily::Stock);
        }
        for i in 1..=300u64 {
            // Contract i trades (301 - i) more units: rank i.
            lb.observe(stock(i, i, 1 + (301 - i as u32)), OptionFamily::Stock);
        }
        let ranked_all = lb.rank(OptionFamily::Stock, S1, usize::MAX, lot1, all);
        assert_eq!(ranked_all.len(), 300, "no cut inside rank");
        // Only the 50 contracts ranked 251..=300 are gainers.
        let (gainers, _) = gainer_eligible(ranked_all, 250, |u| {
            if u > 250 {
                GainerVerdict::Gainer
            } else {
                GainerVerdict::NotGainer
            }
        });
        assert_eq!(gainers.len(), 50);
        assert_eq!(gainers[0].security_id, 251);
    }
}
