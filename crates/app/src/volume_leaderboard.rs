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
//! The heap was premature optimisation against a cost that measurement shows
//! is small, and it bought the single worst failure mode in the design. So
//! this module holds no threshold: [`VolumeLeaderboard::rank`] recomputes from
//! current state, and one bad value affects one contract instead of the whole
//! set.
//!
//! # What a sweep actually costs (MEASURED 2026-09-12, release, x86 dev container)
//!
//! `rank_sweep_cost_at_the_authorized_ceiling`, 20,220 tracked contracts, a
//! real lot-size hash probe per contract, and a board the harness now ASSERTS
//! it filled:
//!
//! | contracts that TRADED in the window | sweep | duty at 1 s |
//! |---|---|---|
//! | 20,220 — every one (the ceiling) | **2.95 ms** | 0.295% |
//! | 2,000 (Assumed realistic) | **123 µs** | 0.012% |
//! | 500 | 28.7 µs | 0.003% |
//! | 100 | 6.4 µs | 0.0006% |
//!
//! The worst SECOND is the one where all four cadence arms land together, for
//! both families: 8 sweeps ≈ **23.6 ms**, a **2.4% duty cycle** on the drain
//! task. At the assumed realistic shape it is ~1 ms, ~0.1%.
//!
//! ⚠ **This header said "A full sweep costs 900 µs at 20,220 contracts — a
//! 0.018% duty cycle" until 2026-09-12, and that number was never measured.**
//! The harness that produced it seeded every baseline to the contract's own
//! volume and then timed fifty sorts of an EMPTY vector, under a single
//! `< 1 s` assertion an empty sort passes trivially. It was quoted onward into
//! a rule file, a plan, `CLAUDE.md`'s O(1) table and this module's own `rank`
//! doc. The true ceiling is **3.3× worse** than the figure it replaced — a
//! stale claim in the reassuring direction, which is the expensive one, inside
//! the harness whose whole purpose was to stop exactly that. The harness now
//! refuses to report a number from a board it did not fill.
//!
//! The 2,000-traded row is **Assumed**: nobody has measured how many of ~20,000
//! strikes trade in one second. The query that would is named in
//! `top_volume_rank_persistence`'s header.
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

/// The persistence cut is this cap, and this pins them together.
///
/// `TOP_VOLUME_PERSIST_PER_FAMILY` means "every traded contract" — which is
/// only TRUE while it is at or above the number of contracts the ranking can
/// hold. Lowering this cap without lowering that one leaves a constant whose
/// doc says "every" and whose value silently cuts; raising this one without
/// raising that one turns the cut back on without anyone editing the cut.
const _: () = assert!(
    tickvault_common::constants::TOP_VOLUME_PERSIST_PER_FAMILY >= MAX_TRACKED_CONTRACTS,
    "the persistence cut sits below the tracked-contract cap, so it is a real \
     cut again — but its doc claims every traded contract is persisted. Move \
     both or neither."
);

/// Counter: observations refused, labelled by reason.
pub const REFUSED_COUNTER: &str = "tv_volume_leaderboard_refused_total";
/// Gauge: contracts currently tracked, per family and cadence.
pub const TRACKED_GAUGE: &str = "tv_volume_leaderboard_tracked";
/// Gauge: size of the last materialised ranking, per family and CADENCE.
///
/// ⚠ The cadence label arrived 2026-09-12, with the third and fourth
/// cadences. Before it the label set was `family` alone, so all cadences
/// wrote the SAME series and the reading was whichever one fired last — on a
/// second where the 1s, 5s and 1m arms all land, three different windows
/// overwrite one number and the operator reads a figure belonging to none of
/// the boards they are looking at. Two cadences aliasing was already wrong;
/// four made it unreadable.
///
/// Costs nothing to fix: neither gauge appears in any file under `deploy/`,
/// so they are served on the local `/metrics` exporter and ship to no
/// CloudWatch series. Going from two series to eight is therefore $0.00/mo
/// and needs no lever under §2.3n of the noise lock — which is also the
/// honest limit of what these gauges are: readable on the box, unalarmable.
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
    /// The rank key's NUMERATOR: units traded in the window that just closed.
    ///
    /// **Written by [`VolumeLeaderboard::rank`], ignored by
    /// [`VolumeLeaderboard::observe`]** — the same contract as
    /// `window_lots_milli` above, for the same reason: it is an output of
    /// the ranking pass, not an observation, and a value set on the way IN
    /// is overwritten.
    ///
    /// Carried so the persisted row can show the division rather than only
    /// its result. `delta` is computed inside `rank` against the cadence's
    /// own baseline and was previously discarded one line later.
    pub delta_units: u32,
    /// The rank key's DENOMINATOR: units per contract, as the ranking used
    /// it.
    ///
    /// Also written by `rank`, from the SAME `lot_of` probe that fed the
    /// division — never a second lookup, which could disagree with the
    /// number actually divided by and would make the stored row a plausible
    /// lie rather than a record.
    pub lot_size: u32,
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
    /// A re-latched contract climbed back to the high it had abandoned, and
    /// the recovery was ABSORBED rather than ranked.
    ///
    /// Without this the climb reads as one window's trading on every cadence
    /// at once — the largest delta the contract will ever show, from a move
    /// that never happened. See [`Tracked::resync_ceiling`].
    ResyncedToCeiling {
        /// The abandoned high the contract climbed back to.
        ceiling: u32,
        /// The volume actually observed, at or above that ceiling.
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
/// one `u32` baseline per cadence plus the one-byte dirty mask, which MEASURES
/// 64 bytes with padding. hashbrown stores the `(key, value)` pair INLINE, so
/// an entry is 16 + 64 = 80 bytes, and the table rounds 25,000 up to 32,768
/// buckets: **~2.6 MB per family, ~5.3 MB in all**, plus ~3.2 MB of work lists
/// (see `Family::dirty`).
///
/// (An earlier version said "8 bytes wider than the contract, ~200 KB per
/// family", counting neither the baselines nor the map. A later one said
/// "~56 bytes … ~1.5–2 MB per family, ~4 MB in all" — closer, but it still
/// missed that hashbrown rounds the bucket count to a power of two, so it
/// understated by ~30%. Corrected 2026-09-12 with the size measured rather
/// than estimated.)
///
/// It buys the difference between a transient mis-ranking and a socket frozen
/// for the session.
#[derive(Debug, Clone, Copy)]
struct Tracked {
    contract: RankedContract,
    /// Reset to 0 on every accepted advance. Only a RUN of lower values counts.
    consecutive_lower: u16,
    /// Cumulative volume as of this contract's last snapshot, PER CADENCE.
    ///
    /// One entry per window because the four boards measure different
    /// intervals: sharing a baseline would make whichever cadence fired last
    /// steal the others' windows, and every board would report a figure no
    /// interval actually traded. The pressure is worse at four cadences than
    /// it was at two — on a second where the 1 s, 5 s and 1 m arms all land,
    /// a shared baseline would give two of the three a window of zero.
    ///
    /// Seeded to the contract's CURRENT volume when it is first tracked, never
    /// to 0. A 0 seed makes the first window report the whole day so far, which
    /// is the largest number that contract will ever show and would hand it a
    /// depth socket on its first appearance — the "stale high" shape this
    /// module already refuses one level up.
    baseline: [u32; WINDOW_COUNT],
    /// Which windows this contract has traded in since their last sweep.
    ///
    /// One BIT per cadence slot. Set on an accepted ADVANCE (and only there —
    /// see `ALL_WINDOWS_DIRTY`), cleared by the sweep that consumes it. The
    /// sweep walks only the contracts whose bit is set, which is what stops the
    /// per-sweep cost scaling with the size of the UNIVERSE instead of with the
    /// number of contracts that actually traded.
    ///
    /// FREE in memory: `Tracked` is 40 B of contract + 2 B of run counter +
    /// `WINDOW_COUNT` u32 baselines, so at four cadences it already pads to 64
    /// and this byte lands in the padding.
    dirty: u8,
    /// The high ABANDONED by the last re-latch, or 0 when none is armed.
    ///
    /// # The phantom this exists to stop
    ///
    /// A re-latch reseeds every baseline to the LOW value it just adopted,
    /// because the stored high was declared garbage and the baselines were
    /// measured against that same garbage. That is correct for the window the
    /// re-latch lands in — it reports 0, which the arm's own comment calls
    /// "the honest answer" — and it says nothing about the window AFTER.
    ///
    /// If the vendor's counter was merely dipping and returns to its true
    /// value, the next advance is accepted normally, the baseline is PRESERVED
    /// at the low, and the following sweep computes `high - low` on EVERY
    /// cadence at once. That is a full-session-sized delta from a move that
    /// never happened: rank 1 on all four boards simultaneously, and a
    /// depth-200 socket handed to it.
    ///
    /// So the ceiling is armed with the abandoned high and consumed by the
    /// first observation that reaches it: the baselines are re-seeded to the
    /// CEILING (not to the new volume — that would discard the genuine trading
    /// that happened at the vendor while we were looking at the dip) and the
    /// recovery is absorbed instead of ranked.
    ///
    /// FREE in memory: `Tracked` was already 64 B with 3 B of tail padding at
    /// `WINDOW_COUNT == 4`, and this lands in it. The `size_of` assert below
    /// is what proves that rather than the comment.
    ///
    /// A volume that never climbs back leaves the ceiling armed for the rest
    /// of the session. That is inert, not a gate: every arm below reads it
    /// only to compare, never to refuse.
    resync_ceiling: u32,
}

/// Distinct snapshot cadences, and therefore baselines per contract.
///
/// Derived from the cadence enum rather than written as a literal, so the
/// array and the enum widen together.
///
/// ⚠ CORRECTED 2026-09-12 — the previous doc here read "adding a third
/// cadence must fail to compile here rather than silently index out of range
/// or, worse, alias two cadences onto one baseline." **It did not**, and the
/// sentence was reassuring in the one direction that costs a session: a
/// variant added to `SnapshotCadence` but not to `SnapshotCadence::ALL`
/// compiled cleanly, left this constant at its old value, and indexed
/// `baseline[2]` into a `[u32; 2]` on the frame drain — `overflow-checks`
/// on, `panic = "abort"`, process gone mid-session. The compile error the
/// sentence was describing came from the SEPARATE `window_index` match, and
/// the obvious way to silence that error is precisely the edit that arms the
/// abort.
///
/// What holds now: the slot is the `#[repr(u8)]` discriminant, the
/// const-assert beside `ALL` proves the list is dense and in order, and
/// `window_slot` bounds-checks the residual case rather than trusting it.
pub const WINDOW_COUNT: usize = SnapshotCadence::ALL.len();

/// Every window bit set — what an accepted advance marks.
///
/// An advance sets ALL of them at once, not just the window that is about to
/// fire, because the contract has traded and therefore has a non-zero delta in
/// EVERY open window simultaneously. The 1s sweep clears only its own bit; the
/// 1m sweep, 59 seconds later, still sees the mark.
const ALL_WINDOWS_DIRTY: u8 = ((1u16 << WINDOW_COUNT) - 1) as u8;

const _: () = assert!(
    WINDOW_COUNT <= 8,
    "the per-contract dirty mask is a u8, one bit per cadence. A ninth cadence \
     needs a wider mask — and silently overflowing it would drop a whole \
     cadence's marks, so its board would report only the contracts that some \
     OTHER cadence happened to mark."
);

/// MEASURED, not assumed: 40 B of contract + 2 B of run counter + four `u32`
/// baselines + the 1 B mask = 59, padded to 64. The mask is genuinely free —
/// it landed in padding that already existed.
///
/// The assert is `<=`, not `==`, so it pins a BOUND rather than today's
/// number: a sixth cadence pushes `Tracked` to 72 and fires it, at which point
/// packing the baselines versus accepting the growth is a decision someone
/// makes rather than one that happens.
///
/// ⚠ CORRECTED 2026-09-12, hours after it was written, by the hot-path panel.
/// This assert's message read "past 64 bytes that is two cache lines instead
/// of one", and that justification is UNSOUND. `HashMap<ContractKey, Tracked>`
/// is hashbrown, which stores the `(K, V)` PAIR inline: 16 + 64 = 80 bytes at
/// align 8, packed contiguously and not 64-aligned. A probe therefore already
/// straddles two lines for most entries, so the single-line benefit the
/// sentence promised was never delivered.
///
/// The assert is KEPT — as a size ratchet it is worth having, because growth
/// here is multiplied by 25,000 entries per family — but it is kept for that
/// reason and not for the one it originally claimed. Recorded rather than
/// silently reworded: a plausible-sounding performance rationale that does not
/// survive contact with the container is exactly the shape this file's header
/// keeps recording.
const _: () = assert!(
    size_of::<Tracked>() <= 64,
    "Tracked is stored 25,000 times per family; growth here is multiplied by \
     that. Decide deliberately before raising this. (NOT a cache-line bound — \
     hashbrown stores the 16-byte key inline beside it, so an entry is already \
     80 bytes and already straddles.)"
);

/// Fixed-point scale on the rank key. 1000 = three decimal places of a lot.
pub const LOTS_SCALE: u64 = 1_000;

const _: () = assert!(
    (u32::MAX as u64).checked_mul(LOTS_SCALE).is_some(),
    "the rank key multiplies a u32 delta by LOTS_SCALE in u64; if that can \
     overflow, a busy contract's key wraps to a small number and it silently \
     leaves the board"
);

/// Which baseline slot a cadence owns.
///
/// ⚠ This was a hand-written `match` until 2026-09-12, and the pairing of
/// that match with `WINDOW_COUNT = SnapshotCadence::ALL.len()` was a live
/// process-abort waiting for a third cadence. The enum doc two screens up
/// claimed a new cadence "must fail to compile here" — it does, and that is
/// exactly the trap: the compiler points at the missing arm, the obvious fix
/// is to write `SnapshotCadence::ThreeSecond => 2`, and THAT compiles while
/// `ALL` still has two entries. `baseline: [u32; 2]` is then written at index
/// 2 on the frame drain, and the release profile is `overflow-checks = true`
/// with `panic = "abort"` — the trading box dies mid-session, on a tick.
///
/// The slot is now the `#[repr(u8)]` discriminant, bounds-checked against
/// `ALL` by [`SnapshotCadence::slot`]. There is no match to keep in step, and
/// the residual case the const-assert cannot rule out (a variant the enum has
/// and `ALL` does not) is a counted refusal instead of an abort.
fn window_slot(cadence: SnapshotCadence) -> Option<usize> {
    let slot = cadence.slot();
    if slot.is_none() {
        // Cold by construction: reaching this means the enum gained a variant
        // that `SnapshotCadence::ALL` does not list, which is a build-time
        // mistake that shipped. Counted and named rather than silent, because
        // the visible symptom would otherwise be one cadence's board quietly
        // never updating.
        metrics::counter!(
            UNSLOTTED_CADENCE_COUNTER,
            "cadence" => cadence.as_str(),
        )
        .increment(1);
        error!(
            code = ErrorCode::WsGapConnectionState.code_str(),
            source = "cadence_without_baseline_slot",
            cadence = cadence.as_str(),
            window_count = WINDOW_COUNT,
            "a snapshot cadence has no baseline slot — it is in SnapshotCadence \
             but not in SnapshotCadence::ALL, so every per-cadence array is one \
             slot short. That cadence's board will not update; nothing else is \
             affected."
        );
    }
    slot
}

/// Counter for the refusal above. Zero on every healthy build — a non-zero
/// reading is a build mistake, not a market condition.
const UNSLOTTED_CADENCE_COUNTER: &str = "tv_volume_leaderboard_unslotted_cadence_total";

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
    /// Keys that have TRADED since each window's last sweep — the work list.
    ///
    /// One list per cadence slot, holding exactly the contracts whose
    /// `Tracked::dirty` bit for that slot is set. The sweep walks this instead
    /// of the whole map, which is what makes the per-sweep cost scale with the
    /// number of contracts that TRADED rather than with the number that EXIST.
    ///
    /// **The invariant, and it is the whole correctness argument:** for every
    /// window `w`, `dirty[w]` contains a key AT MOST ONCE, and contains it if
    /// and only if `volumes[key].dirty & (1 << w) != 0`. Every writer of
    /// `Tracked` upholds it — insert seeds the mask to 0 and pushes nothing,
    /// an accepted advance pushes into exactly the lists whose bit was clear,
    /// a re-latch preserves the mask and pushes nothing, a refusal touches
    /// neither, and a sweep clears its own bit as it drains its own list.
    ///
    /// Pre-sized at construction for the same reason `volumes` is: a list that
    /// grows from empty to the authorized ceiling reallocates and copies ~15
    /// times, and every one of those lands on the per-tick path. The cost is
    /// `WINDOW_COUNT × MAX_TRACKED_CONTRACTS × 16 B` ≈ 1.6 MB per family and
    /// ~3.2 MB in all, committed at boot — stated rather than buried, though
    /// it is small beside the ~5.3 MB the maps already commit (~2.6 MB per
    /// family: hashbrown stores the 16-byte key INLINE beside the 64-byte
    /// `Tracked`, so an entry is 80 B, and 25,000 rounds up to 32,768 buckets
    /// — 32,768 × 80 B ≈ 2.6 MB. An earlier version of this line said "~4 MB",
    /// which counted the value and not the inline key).
    dirty: [Vec<ContractKey>; WINDOW_COUNT],
    non_monotonic: u64,
    at_capacity: u64,
    relatched: u64,
    /// Recoveries absorbed after a re-latch. See `Tracked::resync_ceiling`.
    resynced: u64,
    /// Contracts the sweep put on the work list and then LEFT OFF the board
    /// because their window key truncated to zero milli-lots.
    ///
    /// Added 2026-09-12 by an adversarial sweep, which found this the only
    /// remaining membership filter on `top_volume` with no instrument at all.
    /// See the emit site for what a non-zero reading means -- it is the one
    /// signal that would falsify the unit premise the key rests on.
    zero_lot: u64,
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
    refused_zero_lot: metrics::Counter,
    /// Gauge handles, resolved for the SAME reason as the counters above, and
    /// now ONE PER CADENCE SLOT.
    ///
    /// These sit on the sweep arms rather than the per-tick path, so the
    /// allocating arm would have cost a `Vec` per sweep — real but small.
    /// Resolved anyway because a module whose doc explains this exact mechanism
    /// and then does it is a file the next reader stops trusting.
    ///
    /// Indexed by the cadence's own slot, so the array widens with
    /// `SnapshotCadence::ALL` and a cadence cannot silently share another's
    /// series (see `RANKED_GAUGE`).
    tracked_gauge: [metrics::Gauge; WINDOW_COUNT],
    ranked_gauge: [metrics::Gauge; WINDOW_COUNT],
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
        let refused_zero_lot =
            metrics::counter!(REFUSED_COUNTER, "family" => label, "reason" => "zero_lot_window");
        refused_zero_lot.increment(0);
        // One handle per cadence slot, resolved from `ALL` so the arrays widen
        // with the enum. `from_index` cannot fail for an index below
        // `ALL.len()` — the const-assert beside `ALL` proves the list is dense
        // and in order — but it is matched rather than unwrapped because this
        // runs at boot and an `unwrap` here is a panic on the boot path.
        let tracked_gauge = std::array::from_fn(|i| match SnapshotCadence::from_index(i) {
            Some(cadence) => {
                metrics::gauge!(TRACKED_GAUGE, "family" => label, "cadence" => cadence.as_str())
            }
            None => metrics::gauge!(TRACKED_GAUGE, "family" => label, "cadence" => "unknown"),
        });
        let ranked_gauge = std::array::from_fn(|i| match SnapshotCadence::from_index(i) {
            Some(cadence) => {
                metrics::gauge!(RANKED_GAUGE, "family" => label, "cadence" => cadence.as_str())
            }
            None => metrics::gauge!(RANKED_GAUGE, "family" => label, "cadence" => "unknown"),
        });
        Self {
            // Pre-sized: an unsized map reallocates and rehashes ~15 times on
            // its way to the authorized universe, and every one of those lands
            // on the per-tick path.
            volumes: HashMap::with_capacity(MAX_TRACKED_CONTRACTS),
            // Pre-sized for the same reason, and `from_fn` rather than an
            // array literal because a `Vec` is not `Copy`.
            dirty: std::array::from_fn(|_| Vec::with_capacity(MAX_TRACKED_CONTRACTS)),
            non_monotonic: 0,
            at_capacity: 0,
            relatched: 0,
            resynced: 0,
            zero_lot: 0,
            refused_non_monotonic,
            refused_capacity,
            refused_zero_lot,
            tracked_gauge,
            ranked_gauge,
        }
    }

    /// Clears the tracked contracts and counters, KEEPING the map capacity and
    /// the resolved handles. Replacing the struct would hand back the capacity
    /// and pay the growth again every session.
    fn clear(&mut self) {
        self.volumes.clear();
        // Cleared WITH the map, and it must be: a work list naming keys that
        // no longer exist would have the next sweep probe a cleared map for
        // every one of them — harmless in outcome (`get_mut` misses) but it
        // re-creates the whole-universe walk this list exists to avoid, on the
        // first sweep after every daily reset.
        for window in &mut self.dirty {
            window.clear();
        }
        self.non_monotonic = 0;
        self.at_capacity = 0;
        self.relatched = 0;
        self.resynced = 0;
        self.zero_lot = 0;
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
                        // PRESERVED, and nothing is pushed. The mask and the
                        // work lists are one structure (see `Family::dirty`),
                        // so clearing the mask here without also removing the
                        // key from the lists would let the NEXT advance push a
                        // second copy and the lists would grow past the map.
                        // Preserving costs at most one wasted visit per
                        // re-latch, and that visit computes a delta of 0
                        // because the baseline above was just reseeded to the
                        // very volume it is measured against.
                        dirty: existing.dirty,
                        // ARMED with the high we just stopped defending, and
                        // OVERWRITTEN rather than preserved on a second
                        // re-latch: keeping an older, higher ceiling would
                        // leave a trap that a legitimate later climb trips,
                        // silently eating a real window.
                        resync_ceiling: stored,
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
            // RESYNC PRE-STEP — consume an armed ceiling, then FALL THROUGH to
            // the single advance arm below rather than duplicating it.
            //
            // Placement is the fix: below the `<` and `==` arms so it never
            // fires on a lower or equal value, and above the advance so the
            // phantom delta is never computed.
            //
            // A re-latch reseeded every baseline to the LOW value it adopted.
            // If the vendor's counter was merely DIPPING and has now returned
            // to its true reading, an unadjusted advance would preserve that
            // low baseline and the next sweep would compute `high - low` on
            // ALL FOUR cadences at once — a full-session-sized delta from a
            // move that never happened, ranking this contract first on every
            // board and handing it a depth-200 socket.
            //
            // Reseeding to the CEILING rather than to `contract.volume` is
            // deliberate: the excess above the ceiling is volume that really
            // did trade at the vendor while we were looking at the dip, and
            // crediting it is the honest answer. Only the recovery back TO the
            // abandoned high is absorbed.
            //
            // WHY A PRE-STEP AND NOT ITS OWN ARM: the advance arm is the ONE
            // site that adds to the work list, guarded on the bit, and
            // `the_work_list_has_one_producer_and_the_gauges_are_per_cadence`
            // pins that. An earlier draft of this fix copied the push here and
            // failed that guard — correctly. Mutating the baseline in place
            // and falling through keeps one producer, and the advance arm's
            // own `baseline: existing.baseline` then carries the ceiling
            // forward unchanged.
            let resynced_from =
                if existing.resync_ceiling != 0 && contract.volume >= existing.resync_ceiling {
                    let ceiling = existing.resync_ceiling;
                    existing.baseline = [ceiling; WINDOW_COUNT];
                    // DISARMED. One ceiling, one consumption.
                    existing.resync_ceiling = 0;
                    Some(ceiling)
                } else {
                    None
                };
            // Advance in place. The underlying is refreshed too: a derivative
            // id can be reused across days, and holding a stale underlying
            // would put the contract under the wrong name in the depth-200
            // distinct-underlying constraint.
            let was_dirty = existing.dirty;
            // Copied out BEFORE `contract` moves into `Tracked`, so the resync
            // report at the tail can still name the value that was adopted.
            let volume = contract.volume;
            *existing = Tracked {
                contract,
                consecutive_lower: 0,
                // PRESERVED, deliberately. This is a normal advance inside a
                // window that has not closed yet; reseeding here would zero
                // the delta on every accepted tick and every window would
                // report 0, which is the whole feature silently doing nothing.
                baseline: existing.baseline,
                // EVERY window at once, not just the one about to fire. This
                // contract now has a non-zero delta in all of them
                // simultaneously; marking only the nearest cadence would leave
                // the 1-minute board reporting whichever contracts the
                // 1-second sweep happened not to have cleared yet.
                dirty: ALL_WINDOWS_DIRTY,
                // PRESERVED. A normal advance neither arms nor consumes a
                // ceiling; the arm that consumes it sits ABOVE this one and
                // has already returned if it fired.
                resync_ceiling: existing.resync_ceiling,
            };
            // THE ONLY SITE THAT ADDS WORK. `existing` borrows `volumes` and
            // the lists sit beside it on the same struct, so the mask is
            // copied out above and the borrow ends here before the push.
            //
            // Guarded on the whole mask first: a contract that trades many
            // times between two sweeps is already marked in every window, and
            // that is the COMMON case on a liquid strike — one `u8` compare
            // per tick instead of a loop.
            if was_dirty != ALL_WINDOWS_DIRTY {
                for (window, pending) in slot.dirty.iter_mut().enumerate() {
                    // Push only where the bit was CLEAR, which is what keeps
                    // each key in each list at most once and therefore keeps
                    // the lists bounded by the map they index.
                    if was_dirty & (1u8 << window) == 0 {
                        pending.push(key);
                    }
                }
            }
            // The resync pre-step above already reseeded the baseline and
            // disarmed the ceiling; the advance carried both forward and
            // enrolled the contract through the one producer. All that is left
            // is to REPORT it as an absorption rather than an ordinary
            // advance, so a caller can tell the two apart.
            if let Some(ceiling) = resynced_from {
                slot.resynced = slot.resynced.saturating_add(1);
                let resynced_total = slot.resynced;
                if resynced_total.is_power_of_two() {
                    warn!(
                        metric = REFUSED_COUNTER,
                        family = label,
                        security_id = key.0,
                        segment = ?key.1,
                        ceiling,
                        adopted = volume,
                        resynced_total,
                        "a re-latched contract climbed back to the high it had abandoned — \
                         absorbing the recovery instead of ranking it. Without this the next \
                         sweep would read the whole climb as one window's trading on every \
                         cadence at once and hand the contract a depth socket it did not \
                         earn. Throttled to powers of two per family."
                    );
                }
                return Observation::ResyncedToCeiling {
                    ceiling,
                    adopted: volume,
                };
            }
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
                // CLEAN, and nothing is pushed. The baseline one line above is
                // this contract's own current volume, so its delta in every
                // window is structurally zero until it advances — a sweep that
                // visited it would rank nothing and write a baseline it
                // already holds. Marking it here would put every contract in
                // the universe on the work list at attach and hand back the
                // whole-universe walk on the first sweep of the session.
                dirty: 0,
                // No ceiling is armed: this contract has never re-latched.
                resync_ceiling: 0,
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
    ///
    /// The sort is O(n log n) in that traded population, and since 2026-09-12
    /// so is the WALK that feeds it — the sweep drains `Family::dirty[idx]`
    /// rather than the whole map, so neither half scales with the size of the
    /// universe. That is the achievable form of the operator's O(1) ask:
    /// per-sweep O(1) is arithmetically impossible, because the output is one
    /// row per traded contract and Ω(traded) is a floor; O(1) in the size of
    /// the UNIVERSE is what a work list can deliver, and is what this does.
    ///
    /// ⚠ This doc read "MEASURED at 900 µs for the 20,220-contract worst
    /// case" until 2026-09-12. It was not measured: the harness it cited
    /// seeded every baseline to the contract's own volume and then timed fifty
    /// sorts of an empty vector. Run
    /// `rank_sweep_cost_at_the_authorized_ceiling` — which now REFUSES to
    /// report a number from a board it did not fill — rather than quoting one
    /// from here.
    /// Rolls ONE cadence's baselines forward to the contracts' current
    /// cumulative volume, without ranking.
    ///
    /// # Why this exists (2026-09-09)
    ///
    /// A baseline is seeded at a contract's FIRST `observe`, and until this
    /// existed it was rolled in exactly one place: [`Self::rank`]. But ticks
    /// fold from the candle session open (09:00) while `rank` is gated to the
    /// capture window (09:15), so the first in-window sweep computed
    /// `volume(09:15) - volume(first observe ~09:00)` and reported up to
    /// fifteen minutes of accumulated volume as a single one-second window.
    /// That contract would take a depth socket on a number no other contract
    /// on the board was measured over.
    ///
    /// Calling this on the out-of-window path keeps every baseline tracking
    /// cumulative volume until ranking actually starts, so the first ranked
    /// window measures the window.
    ///
    /// **Honest limit:** the over-count is not believed to be firing today.
    /// NSE runs no pre-open call auction for F&O, a contract carrying
    /// yesterday's trade time is refused as a stale trading day before it is
    /// ever observed, and contracts attach after the window opens anyway. This
    /// converts a property that currently holds because of exchange
    /// microstructure into one that holds by construction.
    ///
    /// O(traded) integer writes, no sort and no allocation — the same walk
    /// `rank` already makes, without the ranking half.
    pub fn roll_baselines(&mut self, family: OptionFamily, cadence: SnapshotCadence) {
        let Some(idx) = window_slot(cadence) else {
            // Refused and counted in `window_slot`. Rolling nothing is the
            // safe half: a baseline that is never rolled makes that cadence
            // report a growing window, which is visible; an out-of-bounds
            // write aborts the process.
            return;
        };
        let slot = self.family_mut(family);
        let mut pending = std::mem::take(&mut slot.dirty[idx]);
        for key in pending.drain(..) {
            let Some(tracked) = slot.volumes.get_mut(&key) else {
                continue;
            };
            tracked.dirty &= !(1u8 << idx);
            tracked.baseline[idx] = tracked.contract.volume;
        }
        // Drained, so this hands the CAPACITY back, not the contents.
        slot.dirty[idx] = pending;
    }

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
        let Some(idx) = window_slot(cadence) else {
            // Refused and counted in `window_slot`. An empty ranking is the
            // honest answer — the alternative is indexing a baseline array
            // that is one slot short, on the frame drain, under
            // `panic = "abort"`.
            return &[];
        };
        // `take` rather than a direct fill: the buffer and the family map both
        // live on `self`, so filling one from the other needs two borrows. Take
        // moves the Vec out WITH its capacity, so the allocation is still made
        // once at construction and reused for the life of the process.
        let mut scratch = std::mem::take(&mut self.scratch);
        scratch.clear();

        // ONE pass over the contracts that TRADED in this window, and it does
        // three things that must not be split apart: computes each contract's
        // delta against ITS baseline, pushes the eligible ones, and rolls
        // EVERY TRADED baseline forward — ranked or not.
        //
        // "Every traded, ranked or not" is the load-bearing phrase, and the
        // baseline roll below sits ABOVE the eligibility test for exactly that
        // reason. Rolling forward only the RANKED contracts would let an
        // ineligible one accumulate across windows and arrive with a delta
        // measuring minutes the moment it became eligible — it would take a
        // depth socket on a number no other contract on the board was measured
        // over.
        //
        // ⚠ 2026-09-12 — this comment said "rolls EVERY baseline forward" and
        // the loop walked the whole map. It now walks the work list, and the
        // two are EQUIVALENT rather than merely close:
        //
        //   a contract is absent from `dirty[idx]` ⟺ its bit for `idx` is
        //   clear ⟺ no accepted advance since this window's last sweep ⟹
        //   `baseline[idx] == volume` ⟹ `delta == 0` ⟹ `lots == 0` ⟹ the
        //   loop would have `continue`d, after writing a baseline it already
        //   held.
        //
        // The arrows are DIRECTIONAL past the third step and that is not a
        // typographic nicety — the last two do not hold in reverse.
        // `lots == delta * 1000 / lot_size` is an integer division, so a
        // delta of 1 on a 2,000-unit lot yields `lots == 0` with `delta != 0`;
        // and the loop also `continue`s when the lot size is MISSING, whatever
        // the delta. Neither reverse direction is needed: the claim being
        // proven is one-way — absent from the list ⟹ the old walk would have
        // skipped it — so left-to-right is the whole proof. (Stated because an
        // earlier version of this comment wrote ⟺ throughout, which asserts
        // two things that are false and would mislead anyone auditing the
        // skip by reading the chain rather than the code.)
        //
        // The middle step is what every writer of `Tracked` is arranged to
        // keep true: insert seeds the baseline to the current volume, an
        // accepted advance marks the bit, a re-latch reseeds the baseline AND
        // preserves the mask, and a refusal changes neither the volume nor the
        // baseline. So skipping an unmarked contract is not an approximation
        // of the old walk — it produces the same board, byte for byte, and
        // the same stored baselines. Pinned by
        // `a_contract_that_traded_is_never_skipped_by_the_sweep_that_follows`.
        let slot = self.family_mut(family);
        let mut pending = std::mem::take(&mut slot.dirty[idx]);
        for key in pending.drain(..) {
            // A key on the list whose entry is gone can only mean a daily
            // reset landed between the mark and the sweep, and `clear` empties
            // both halves together — so this is the defensive arm, and it
            // skips rather than re-inserting a contract the reset removed.
            let Some(tracked) = slot.volumes.get_mut(&key) else {
                continue;
            };
            tracked.dirty &= !(1u8 << idx);
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
            //
            // Split into two `let-else` steps rather than one `and_then` so
            // the lot size SURVIVES the expression: it is the denominator the
            // division actually used, and the persisted row carries it. Both
            // arms still `continue`, so the short-circuit behaviour is
            // byte-identical to the chained form it replaced.
            // CARRIED, not probed. The drain populates this from the
            // `ContractOwner` it already looks up on every tick, so in
            // production the sweep never goes back to the global map for it —
            // which is ~40% of the ceiling sweep (1.78 ms without the probe,
            // 2.95 ms with it, at 20,220 traded).
            //
            // Zero means nothing carried one, and the probe is the FALLBACK
            // rather than a skip: a caller that does not populate the field
            // behaves exactly as it did before this became an input. Both
            // arms still `continue` on a missing lot, so the short-circuit is
            // unchanged.
            let lot = match row.lot_size {
                0 => {
                    let Some(probed) = lot_of(&row) else {
                        continue;
                    };
                    probed
                }
                carried => carried,
            };
            let Some(lots) = window_lots_milli(delta, lot) else {
                continue;
            };
            // A contract whose window key truncates to ZERO milli-lots is not
            // "top volume" and is left OFF the board — the same treatment as a
            // missing lot size, for the same reason: ranking zeros would pad
            // the tail of a 250-deep board with contracts ordered by nothing
            // but their security_id, and the depth pools would swap sockets
            // onto strikes that traded nothing. Found by the 2026-09-08
            // adversarial sweep; before it, the first sweep after every boot
            // published up to 250 zero-lot "top" contracts.
            //
            // ⚠ CORRECTED 2026-09-12 — this comment used to justify the skip
            // by saying a zero is "the COMMON value (every quiet contract,
            // every first sweep after a boot)". That was true of the
            // whole-map walk it was written for and is FALSE of the work-list
            // drain that replaced it: a quiet contract has a CLEAR dirty bit
            // and never reaches this line. Everything on this list traded.
            //
            // So a zero here is now RARE, and its two remaining causes are
            // both worth seeing:
            //
            //   (a) `delta × 1000 < lot_size` — the division truncated. On a
            //       1,800-unit lot that needs delta < 2 units, which should be
            //       vanishingly rare IF `volume` is in the same unit as
            //       `lot_size`. **That premise is an INFERENCE, not a vendor
            //       fact**: the Dhan doc gives the field as `| 23-26 | int32 |
            //       4 | Volume |` with no unit, and this module's header flags
            //       the CUMULATIVE premise at length while never flagging the
            //       UNIT one. If volume actually arrives in LOTS, this key
            //       becomes roughly `lots / lot_size` — a systematic penalty on
            //       large-lot contracts, with every downstream surface still
            //       green. A SUSTAINED non-zero count here, concentrated on
            //       large-lot contracts, is that signature.
            //   (b) a resync that adopted EXACTLY the abandoned ceiling, so
            //       every window delta is genuinely zero for one sweep.
            //
            // The counter is what lets the next live session tell (a) from (b)
            // and from "the filter never fires". The settling measurement is
            // one query on a live box, and it is stated at the warn below.
            if lots == 0 {
                slot.zero_lot = slot.zero_lot.saturating_add(1);
                slot.refused_zero_lot.increment(1);
                let zero_lot_total = slot.zero_lot;
                // Throttled on powers of two: a session can hit this many
                // times, and the 1st/2nd/4th... report both the ONSET and the
                // MAGNITUDE without flooding the sink. The counter NAME is a
                // field so an operator grepping it lands here — and so the
                // loss-counter visibility guard can see this loss-shaped
                // counter has a surface at all.
                if zero_lot_total.is_power_of_two() {
                    warn!(
                        metric = REFUSED_COUNTER,
                        family = family.as_str(),
                        reason = "zero_lot_window",
                        security_id = key.0,
                        segment = ?key.1,
                        delta_units = delta,
                        lot_size = lot,
                        zero_lot_total,
                        "volume_leaderboard: a contract that TRADED in this window ranked zero milli-lots and was left off the board. Rare by design. If this is sustained and concentrated on large lot sizes, the ranking key's unit premise is wrong -- settle it on a live box with: SELECT delta_units, lot_size FROM top_volume WHERE tf='1s' LIMIT 50. delta_units a multiple of lot_size means volume arrives in LOTS and the key is inverted; unrelated small values mean the premise holds."
                    );
                }
                continue;
            }
            row.window_lots_milli = lots;
            // The two inputs the key was computed from, recorded beside it so
            // a reader of the table can redo the division instead of trusting
            // it. Set from the same `delta` and `lot` the line above divided —
            // not re-derived, which could drift from what was ranked.
            row.delta_units = delta;
            row.lot_size = lot;
            if eligible(&row) {
                scratch.push(row);
            }
        }
        // Drained, so this hands the CAPACITY back, not the contents.
        slot.dirty[idx] = pending;

        // Most lots traded in the window first, then a DETERMINISTIC tiebreak
        // on the composite identity. Without the tiebreak, equal keys order by
        // whatever the hash map yielded, so the same input reorders between
        // sweeps and the caller swaps subscriptions for nothing — and equal
        // keys are COMMON now in a way they were not under a cumulative key:
        // every contract that traded nothing in the window is exactly 0.
        //
        // THIS IS THE VOLUME-PERCENTAGE ORDER (operator 2026-09-12: "rank on
        // volume-percentage alone"). The key is `window_lots_milli` and the
        // percentage the operator reads is
        // `net_volume_chg_pct = window_lots_milli / 10 - 100` — a strictly
        // increasing affine transform, so the two produce the SAME sequence,
        // row for row, including every tie. Sorting on the percentage instead
        // would change nothing except to put a float in a comparator, and a
        // non-finite comparator is non-transitive: one NaN corrupts the whole
        // sort rather than misplacing one row. Integer key, percentage
        // presentation. Pinned by
        // `ranking_by_volume_percentage_is_the_same_order_as_ranking_by_lots`.
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
        // Indexed by THIS sweep's slot. Sharing one handle across cadences
        // made the reading whichever arm fired last — see `RANKED_GAUGE`.
        slot.tracked_gauge[idx].set(slot.volumes.len() as f64);
        slot.ranked_gauge[idx].set(ranked_len as f64);
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

    /// Recoveries ABSORBED after a re-latch, for a family.
    ///
    /// A non-zero value means the vendor's counter was DIPPING, not resetting:
    /// the contract climbed back to the high the re-latch abandoned, and that
    /// climb was credited to the baseline instead of being ranked as one
    /// window's trading. Each one is a phantom rank-1 that did NOT happen.
    ///
    /// Deliberately NOT a new metric name, for the same reason `relatches`
    /// is not: the event carries a `warn!` naming the contract, the ceiling
    /// and the adopted value, and a second series costs ~$0.30/mo against a
    /// September forecast of $142.24 with the automatic `STOP_EC2_INSTANCES`
    /// line at $135.00.
    #[must_use]
    pub const fn resyncs(&self, family: OptionFamily) -> u64 {
        self.family_ref(family).resynced
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

/// The segment this family's UNDERLYING trades in.
///
/// # Why this exists rather than a constant at each call site
///
/// [`STOCK_OPTION_UNDERLYING_SEGMENT`] is right for the eligibility filter,
/// which is Stock-only by the 2026-09-06 lock. It is WRONG for the persisted
/// `top_volume_rank` rows, which are written for BOTH families — and until
/// 2026-09-09 the `gain_pct` closure used it unconditionally.
///
/// An index option's `underlying_id` is an index id (NIFTY=13, BANKNIFTY=25,
/// SENSEX=51). Probing `(13, NSE_EQ)` is exactly the I-P1-11 collision this
/// repository bans: at best it finds nothing and every index row is refused
/// `NonFiniteGain` so the index half of the table is empty; at worst an NSE
/// cash equity carries id 13 — low ids are where equities live — and the row
/// is ACCEPTED carrying that stock's percentage under an index's name, with
/// no NaN to catch it and no refusal counter moving.
///
/// Deriving the segment from the family makes the wrong pairing
/// unrepresentable instead of merely corrected.
#[must_use]
pub const fn underlying_segment(family: OptionFamily) -> ExchangeSegment {
    match family {
        OptionFamily::Index => ExchangeSegment::IdxI,
        OptionFamily::Stock => STOCK_OPTION_UNDERLYING_SEGMENT,
    }
}

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

/// The UNDERLYING's percent change — the same number, from the same two
/// inputs, that [`underlying_gainer_verdict`] turns into a boolean.
///
/// # Why this exists and why it takes the verdict's inputs, not its own
///
/// The persisted `top_volume_rank.gain_pct` and the gainer FILTER must never
/// be able to disagree. If the column were computed from a different source
/// than the verdict, a row could read `+5.0` and still have been filtered out
/// as not-a-gainer, and no reader could tell which half was wrong. Taking the
/// identical `(spot_paise, prev_close)` pair makes that contradiction
/// unrepresentable: the sign of this value IS the verdict.
///
/// # Why the UNDERLYING and not the contract
///
/// Until 2026-09-09 the snapshot asked for the CONTRACT's own previous close.
/// Nothing has ever written one — `record_prev_close_from_tick` stores only
/// [`STOCK_OPTION_UNDERLYING_SEGMENT`] — so the probe returned `None` on every
/// row, became `NaN`, and the projection refused the row as `NonFiniteGain`.
/// **Every row of every snapshot was dropped, so the table was empty for the
/// whole session while every counter read healthy.**
///
/// Feeding the underlying is also what [`eligible_gain_pct`] was written for:
/// its own [`MAX_PLAUSIBLE_GAIN_PCT`] doc says a deep-out-of-the-money option
/// "can move that far" and is "not what this function is fed". So the old call
/// site was wrong twice — a NaN in the ordinary case, and a bound that would
/// have refused a legitimately explosive strike in the case where a contract
/// close did exist.
///
/// O(1): two `Option` unwraps, one integer compare, one divide. Cold — once
/// per ranked row on the snapshot arm, never per tick.
#[must_use]
pub fn underlying_gain_pct(spot_paise: Option<i64>, prev_close: Option<f64>) -> Option<f64> {
    let (Some(spot), Some(close_paise)) = (
        spot_paise,
        prev_close.and_then(crate::spot_price_store::rupees_to_paise),
    ) else {
        return None;
    };
    if spot <= 0 || close_paise <= 0 {
        return None;
    }
    // Paise on BOTH sides. A ratio is scale-invariant, so this is the same
    // percentage the rupee values give, without a second rounding. Both are
    // bounded by `rupees_to_paise`'s own ceiling, far inside f64's exact
    // integer range, so neither widening loses a digit.
    eligible_gain_pct(spot as f64, close_paise as f64)
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
    // Finite is not the same as plausible. A previous close of 0.01 against
    // a 240-rupee LTP yields +2,400,000% — finite, sortable, and nonsense —
    // and it was persisted as a DOUBLE until 2026-09-08 (identity sweep row
    // 22). No NSE stock or option moves ten-fold in a session against its
    // own previous close; a value past the bound is a corrupt input, not a
    // move, and is refused the same way a NaN is.
    if pct.is_finite() && pct.abs() <= MAX_PLAUSIBLE_GAIN_PCT {
        Some(pct)
    } else {
        None
    }
}

/// The widest percent move against the previous close that is treated as a
/// real reading rather than a corrupt previous close.
///
/// 1,000% is a ten-fold move in one session. Stocks with F&O have no daily
/// circuit band, but a ten-fold move has never happened on the NSE cash
/// segment in a session, and deep-out-of-the-money OPTIONS — which can move
/// that far — are not what this function is fed: the gainer verdict reads
/// the UNDERLYING's spot against the underlying's previous close.
pub const MAX_PLAUSIBLE_GAIN_PCT: f64 = 1_000.0;

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
            delta_units: 0,
            lot_size: 0,
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

    /// Ticks fold from the candle session open (09:00) but ranking is gated to
    /// the capture window (09:15). Without an out-of-window baseline roll, the
    /// FIRST in-window sweep measures from the contract's first observe and
    /// reports ~15 minutes of volume as one one-second window.
    #[test]
    fn roll_baselines_out_of_window_makes_the_first_ranked_window_measure_the_window() {
        let mut lb = VolumeLeaderboard::new();

        // 09:00 — first observe seeds the baseline at this volume.
        lb.observe(stock(1, 100, 1_000), OptionFamily::Stock);
        // 09:00 -> 09:15, pre-open accumulation nobody ranked.
        lb.observe(stock(1, 100, 900_000), OptionFamily::Stock);

        // What the out-of-window path now does on every skipped sweep.
        lb.roll_baselines(OptionFamily::Stock, S1);

        // 09:15 — the first RANKED window trades 500 units.
        lb.observe(stock(1, 100, 900_500), OptionFamily::Stock);
        let ranked = lb.rank(OptionFamily::Stock, S1, 10, lot1, all);
        assert_eq!(ranked.len(), 1);
        assert_eq!(
            ranked[0].window_lots_milli, 500_000,
            "the first ranked window must measure the WINDOW (500 units at lot 1 \
             = 500_000 milli-lots), not the ~899,500 accumulated before ranking \
             started"
        );
    }

    /// The operator's requirement is that the board be ranked "purely based on
    /// volume percentage". It already is, and this proves it rather than
    /// asserting it.
    ///
    /// `net_volume_chg_pct = window_lots_milli / 10 - 100` is strictly
    /// increasing, so ordering by it and ordering by the stored integer key
    /// produce the same sequence — including ties, which are common (every
    /// contract that traded nothing in a window is exactly 0 lots). This test
    /// ranks a board, then independently re-sorts the SAME rows by the
    /// percentage as a float, and requires the two orders to be identical.
    ///
    /// It is what lets the module claim the percentage ordering without
    /// putting a float in the comparator, and it is the reason no redundant
    /// percentage COLUMN is stored: a second copy of a total function of an
    /// existing column would cost ~8 B on every one of the millions of rows a
    /// session now writes, on a box whose disk burn already caused a
    /// zero-capture day, to record a number the view derives exactly.
    #[test]
    fn ranking_by_volume_percentage_is_the_same_order_as_ranking_by_lots() {
        let mut lb = VolumeLeaderboard::new();
        // A spread that exercises both sides of the +-0% crossing: under one
        // lot is NEGATIVE percent, exactly one lot is 0%, above is positive.
        // Ties included deliberately — the tiebreak must survive the transform.
        // Lot size 200, so a window delta BELOW 200 units is less than one lot
        // and reads as a negative change. Seeded non-zero: a contract is first
        // tracked at the volume it is first seen with, so a 0 seed would make
        // every delta 0 and the board empty.
        let lot200 = |_: &RankedContract| Some(200_u32);
        for (id, delta) in [
            (1_u64, 50_u32), // a quarter lot  -> -75%
            (2, 200),        // exactly one lot ->   0%
            (3, 200),        // tie with 2
            (4, 6_400),      // 32 lots        -> +3100%
            (5, 1),          // one unit       -> -99.5%
            (6, 0),          // never traded   -> dropped at the zero-lot skip
            (7, 500),        // 2.5 lots       -> +150%
            (8, 6_400),      // tie with 4
        ] {
            lb.observe(stock(id, 100, 1_000), OptionFamily::Stock);
            lb.observe(stock(id, 100, 1_000 + delta), OptionFamily::Stock);
        }
        let by_lots = lb
            .rank(OptionFamily::Stock, S1, usize::MAX, lot200, all)
            .to_vec();
        assert!(
            by_lots.len() >= 6,
            "the fixture must produce a real board, got {}",
            by_lots.len()
        );

        // The exact expression `console_views` puts in the view.
        fn chg_pct(milli: u64) -> f64 {
            milli as f64 / 10.0 - 100.0
        }

        let mut by_pct = by_lots.clone();
        by_pct.sort_by(|a, b| {
            chg_pct(b.window_lots_milli)
                .partial_cmp(&chg_pct(a.window_lots_milli))
                .expect("the fixture contains no NaN — which is the point")
                .then_with(|| a.security_id.cmp(&b.security_id))
                .then_with(|| (a.segment as u8).cmp(&(b.segment as u8)))
        });

        let lots_order: Vec<u64> = by_lots.iter().map(|r| r.security_id).collect();
        let pct_order: Vec<u64> = by_pct.iter().map(|r| r.security_id).collect();
        assert_eq!(
            lots_order, pct_order,
            "ordering by the stored lots key and by the displayed volume \
             percentage must be the SAME sequence; if they ever differ, the \
             claim that the board is ranked on volume percentage is false"
        );

        // And the percentage really is a CHANGE from one lot, not a ratio of
        // one lot: half a lot must read negative, not a reassuring 50%.
        let half = by_lots
            .iter()
            .find(|r| r.security_id == 1)
            .expect("the half-lot contract must be on the board");
        assert!(
            chg_pct(half.window_lots_milli) < 0.0,
            "half a lot must read as a NEGATIVE change, got {}",
            chg_pct(half.window_lots_milli)
        );
    }

    /// Asserts `Family::dirty`'s invariant directly, on every family, over
    /// every shape `observe` can leave behind.
    ///
    /// For each window `w`: the work list holds each key AT MOST ONCE, holds
    /// it if and only if the contract's bit for `w` is set, and every tracked
    /// contract ABSENT from the list has `baseline[w] == volume`.
    ///
    /// That last clause is the whole correctness argument for walking the list
    /// instead of the map — it says the sweep's visit to a skipped contract
    /// would have computed a delta of zero and written back a baseline it
    /// already held.
    fn assert_dirty_invariant(lb: &VolumeLeaderboard, family: OptionFamily, note: &str) {
        let slot = lb.family_ref(family);
        for (window, pending) in slot.dirty.iter().enumerate() {
            let bit = 1u8 << window;
            let mut seen = std::collections::HashSet::new();
            for key in pending {
                assert!(
                    seen.insert(*key),
                    "{note}: {family:?} window {window} lists {key:?} twice — the lists \
                     would grow past the map they index"
                );
                // Not `if let Some(..)`: an entry on the list whose map entry
                // is GONE is exactly the "lists grow past the map" failure the
                // duplicate check above names, and a conditional would accept
                // it in silence. Unreachable today — `volumes` has no `remove`
                // and `clear` empties both halves together — so this asserts a
                // property rather than guarding a live path, which is what a
                // test helper is for.
                let tracked = slot.volumes.get(key).unwrap_or_else(|| {
                    panic!(
                        "{note}: {family:?} window {window} lists {key:?} with no map entry \
                         — the list has outlived the map it indexes"
                    )
                });
                assert_ne!(
                    tracked.dirty & bit,
                    0,
                    "{note}: {family:?} window {window} lists {key:?} but its bit is clear"
                );
            }
            for (key, tracked) in &slot.volumes {
                if tracked.dirty & bit != 0 {
                    assert!(
                        seen.contains(key),
                        "{note}: {family:?} window {window} has {key:?} marked but NOT on the \
                         work list — the sweep would never visit it and its traded volume \
                         would be folded into a later window"
                    );
                    continue;
                }
                assert_eq!(
                    tracked.baseline[window], tracked.contract.volume,
                    "{note}: {family:?} window {window} skips {key:?}, so its delta must be \
                     structurally zero — a non-zero delta here is volume the board silently \
                     loses"
                );
            }
        }
    }

    /// The work list has exactly ONE producer, and the sweeps are the only
    /// consumers.
    ///
    /// `assert_dirty_invariant` proves the structure is consistent after the
    /// operations the tests drive. It cannot prove that some FUTURE call site
    /// does not push from somewhere else — and a second producer is precisely
    /// how the "at most once" half breaks, because the bit guard that keeps a
    /// key unique lives at the one site that owns it. So the site count is
    /// pinned in the source.
    ///
    /// Also pins the per-cadence gauges: sharing one handle across cadences is
    /// invisible at runtime (every `set` succeeds) and makes the reading
    /// whichever arm fired last, which is a wrong number rather than a missing
    /// one.
    #[test]
    fn the_work_list_has_one_producer_and_the_gauges_are_per_cadence() {
        let src = include_str!("volume_leaderboard.rs");
        let production = src
            .split("\n#[cfg(test)]")
            .next()
            .expect("production text precedes the first test module");

        assert_eq!(
            production.matches(".push(key)").count(),
            1,
            "exactly ONE site may add to the work list — the accepted-advance arm, which is \
             the only place that checks the bit before pushing. A second producer breaks the \
             at-most-once half of the invariant and the lists grow past the map they index"
        );
        // ⚠ The assertion above counts ONE SPELLING, and a hostile review of
        // this change pointed out that `pending.push(k)` or `list.push(*key)`
        // walks straight past it. So the TOTAL push count is pinned too: any
        // new `.push(` anywhere in the production half fails this test and the
        // author has to come here and say which list they are pushing into.
        //
        // The five are: the work-list producer (the only one that matters
        // here), `scratch.push(row)` in `rank`, `seen.push`/`out.push` in
        // `distinct_underlying_over`, and `out.push` in `gainer_eligible` —
        // the last four all push into function-local `Vec`s that die at the
        // end of the call and index nothing.
        assert_eq!(
            production.matches(".push(").count(),
            5,
            "a new `.push(` appeared in the production half. If it pushes into a work list \
             it is a SECOND PRODUCER and breaks the at-most-once invariant; if it pushes \
             into a function-local buffer it is harmless — decide which, then update this \
             count and the list above. The spelling-specific assertion directly above \
             cannot see a push written any other way, which is why this one is here"
        );
        // And that site is guarded on the bit, which is what makes it
        // at-most-once rather than merely rare.
        assert!(
            production.contains("if was_dirty & (1u8 << window) == 0 {"),
            "the push must be guarded on the window's bit being CLEAR"
        );
        // Exactly two drains: `rank` and `roll_baselines`. A third consumer
        // that takes the list without clearing the matching bits would leave
        // contracts marked-but-unlisted, which is a trade silently folded into
        // a later window.
        assert_eq!(
            production
                .matches("std::mem::take(&mut slot.dirty[idx])")
                .count(),
            2,
            "only `rank` and `roll_baselines` may drain a work list"
        );
        // Same evasion, same close: the assertion above matches one spelling,
        // so `slot.dirty[i]` or `slot.dirty[idx].drain(..)` would be a THIRD
        // consumer it cannot see. Pinning every access to a work-list slot
        // catches any of them. The four are the take and the restore inside
        // each of the two drains.
        assert_eq!(
            production.matches("slot.dirty[").count(),
            4,
            "a new access to a work-list slot appeared. Only `rank` and `roll_baselines` may \
             touch one, and each does exactly twice — take, then restore. A third consumer \
             that drains without clearing the matching bits leaves contracts marked-but-\
             unlisted, which is a trade silently folded into a later window"
        );
        assert_eq!(
            production
                .matches("tracked.dirty &= !(1u8 << idx);")
                .count(),
            2,
            "and each drain must clear the bit it consumes, or the contract stays marked \
             with nothing on the list to visit it"
        );

        // Scoped to the gauge macros, not to the bare label text — the
        // unslotted-cadence counter carries the same label for its own reasons
        // and must not be what satisfies this.
        for gauge in ["TRACKED_GAUGE", "RANKED_GAUGE"] {
            assert_eq!(
                production
                    .matches(&format!("{gauge}, \"family\" => label, \"cadence\" =>"))
                    .count(),
                2,
                "{gauge} must carry a cadence label on BOTH resolution arms; without it four \
                 cadences write one series and the operator reads whichever arm fired last"
            );
        }
        assert!(
            production.contains("slot.tracked_gauge[idx].set(")
                && production.contains("slot.ranked_gauge[idx].set("),
            "and each sweep must write the handle for ITS OWN cadence slot"
        );
    }

    /// The sweep walks the work list rather than the map, and this is why that
    /// is sound rather than merely cheap.
    ///
    /// Drives every state `observe` can leave a contract in — freshly
    /// inserted, advanced, advanced-then-swept, refused as non-monotonic,
    /// unchanged, and re-latched — and checks the invariant after each, on
    /// every cadence, so an off-by-one in the slot arithmetic cannot hide in
    /// the cadence a pairwise test did not name.
    #[test]
    fn a_contract_that_traded_is_never_skipped_by_the_sweep_that_follows() {
        let mut lb = VolumeLeaderboard::new();

        // Freshly inserted: baseline seeded to its own volume, nothing marked.
        lb.observe(stock(1, 100, 5_000), OptionFamily::Stock);
        assert_dirty_invariant(&lb, OptionFamily::Stock, "after insert");
        assert!(
            lb.family_ref(OptionFamily::Stock).dirty[0].is_empty(),
            "an insert must not put the contract on the work list — doing so hands back the \
             whole-universe walk on the first sweep of the session"
        );

        // Advanced: marked in EVERY window at once.
        lb.observe(stock(1, 100, 5_400), OptionFamily::Stock);
        assert_dirty_invariant(&lb, OptionFamily::Stock, "after advance");
        for window in 0..WINDOW_COUNT {
            assert_eq!(
                lb.family_ref(OptionFamily::Stock).dirty[window].len(),
                1,
                "an advance marks window {window} too, not just the cadence about to fire"
            );
        }

        // Advancing again must NOT push a second copy.
        lb.observe(stock(1, 100, 5_900), OptionFamily::Stock);
        assert_dirty_invariant(&lb, OptionFamily::Stock, "after a second advance");
        assert_eq!(
            lb.family_ref(OptionFamily::Stock).dirty[0].len(),
            1,
            "a contract that trades repeatedly between sweeps must stay on the list once"
        );

        // Swept on ONE cadence: cleared there, still pending everywhere else.
        let ranked = lb.rank(OptionFamily::Stock, S1, 10, lot1, all);
        assert_eq!(ranked.len(), 1, "the traded contract must be on the board");
        assert_eq!(
            ranked[0].delta_units, 900,
            "and it must carry the window it actually traded"
        );
        assert_dirty_invariant(&lb, OptionFamily::Stock, "after the 1s sweep");
        assert!(
            lb.family_ref(OptionFamily::Stock).dirty[0].is_empty(),
            "the swept window's list is drained"
        );
        assert_eq!(
            lb.family_ref(OptionFamily::Stock).dirty[1].len(),
            1,
            "and the OTHER windows still hold it — the 1m board must not lose a trade \
             because the 1s board swept first"
        );

        // PARTIALLY marked, and this is the state production spends almost all
        // its time in: the 1s sweep has just cleared bit 0 while the 3s, 5s and
        // 1m bits are still set, and then the contract trades again. The push
        // must land in the ONE list that lost it and nowhere else — a push into
        // all four would duplicate it in three of them, and the lists would
        // grow past the map they index. (Added 2026-09-12: the first version of
        // this test advanced only from the fully-clear and fully-marked states,
        // so removing the per-window bit guard passed it.)
        lb.observe(stock(1, 100, 6_500), OptionFamily::Stock);
        assert_dirty_invariant(
            &lb,
            OptionFamily::Stock,
            "after advancing while partly marked",
        );
        for window in 0..WINDOW_COUNT {
            assert_eq!(
                lb.family_ref(OptionFamily::Stock).dirty[window].len(),
                1,
                "advancing from a PARTLY marked state must leave window {window} holding the \
                 contract exactly once"
            );
        }
        // Consume it again so the states below start from a swept window.
        let _ = lb.rank(OptionFamily::Stock, S1, 10, lot1, all);

        // Unchanged and refused: neither may touch the lists.
        lb.observe(stock(1, 100, 5_900), OptionFamily::Stock);
        lb.observe(stock(1, 100, 10), OptionFamily::Stock);
        assert_dirty_invariant(&lb, OptionFamily::Stock, "after unchanged + refused");
        assert!(
            lb.family_ref(OptionFamily::Stock).dirty[0].is_empty(),
            "an unchanged or refused observation records nothing, so it marks nothing"
        );

        // Re-latched: the mask is preserved, so the lists stay exact.
        for _ in 0..RELATCH_AFTER_CONSECUTIVE_LOWER {
            lb.observe(stock(1, 100, 10), OptionFamily::Stock);
        }
        assert_eq!(
            lb.relatches(OptionFamily::Stock),
            1,
            "the fixture must actually reach the re-latch it is testing"
        );
        assert_dirty_invariant(&lb, OptionFamily::Stock, "after relatch");

        // And a sweep of every cadence, in turn, leaves it consistent.
        for cadence in SnapshotCadence::ALL {
            lb.observe(stock(2, 200, 1), OptionFamily::Stock);
            lb.observe(stock(2, 200, 7_000), OptionFamily::Stock);
            let _ = lb.rank(OptionFamily::Stock, cadence, 10, lot1, all);
            assert_dirty_invariant(&lb, OptionFamily::Stock, "after a full-cadence sweep");
            lb.roll_baselines(OptionFamily::Stock, cadence);
            assert_dirty_invariant(&lb, OptionFamily::Stock, "after a roll");
        }

        // The daily reset empties both halves together. Trade FIRST, so there
        // is something on the lists for the reset to forget — without this the
        // assertion below holds whether `clear` empties them or not, which is
        // how the first version of this test passed with the reset's list-clear
        // deleted (2026-09-12).
        lb.observe(stock(3, 300, 400), OptionFamily::Stock);
        lb.observe(stock(3, 300, 900), OptionFamily::Stock);
        assert!(
            !lb.family_ref(OptionFamily::Stock).dirty[0].is_empty(),
            "the fixture must leave work pending, or the reset assertion below proves nothing"
        );
        lb.reset_daily();
        assert_dirty_invariant(&lb, OptionFamily::Stock, "after reset");
        assert!(
            lb.family_ref(OptionFamily::Stock).dirty[0].is_empty(),
            "a work list naming keys the reset removed re-creates the whole-universe probe"
        );
    }

    /// The dirty-set sweep must produce the SAME board as a walk over every
    /// tracked contract — not a close one.
    ///
    /// Builds a population that is mostly quiet (the realistic shape: a few
    /// hundred of ~20,000 strikes trade in any one second), sweeps it, and
    /// compares against the board recomputed by hand from every contract in
    /// the map. Byte for byte, including the tiebreak order.
    #[test]
    fn the_dirty_sweep_ranks_exactly_what_a_full_walk_would() {
        let mut lb = VolumeLeaderboard::new();
        // 400 contracts attach; only every seventh one then trades.
        for id in 0..400u64 {
            lb.observe(stock(id, id % 20, 1_000 + id as u32), OptionFamily::Stock);
        }
        // One sweep to establish steady state, so the comparison is not run
        // against the special first-sweep case.
        let _ = lb.rank(OptionFamily::Stock, S1, usize::MAX, lot1, all);

        let mut traded = 0usize;
        for id in (0..400u64).step_by(7) {
            lb.observe(
                stock(id, id % 20, 1_000 + id as u32 + (id as u32 % 13) + 1),
                OptionFamily::Stock,
            );
            traded += 1;
        }
        assert!(traded > 20, "the fixture must trade a real fraction");

        // What a FULL walk over the map would have ranked, computed here.
        let mut expected: Vec<RankedContract> = lb
            .family_ref(OptionFamily::Stock)
            .volumes
            .values()
            .filter_map(|t| {
                let delta = t.contract.volume.saturating_sub(t.baseline[0]);
                let lots = window_lots_milli(delta, 1)?;
                if lots == 0 {
                    return None;
                }
                let mut row = t.contract;
                row.window_lots_milli = lots;
                row.delta_units = delta;
                row.lot_size = 1;
                Some(row)
            })
            .collect();
        expected.sort_unstable_by(|a, b| {
            b.window_lots_milli
                .cmp(&a.window_lots_milli)
                .then_with(|| a.security_id.cmp(&b.security_id))
                .then_with(|| (a.segment as u8).cmp(&(b.segment as u8)))
        });

        let actual = lb
            .rank(OptionFamily::Stock, S1, usize::MAX, lot1, all)
            .to_vec();
        assert_eq!(
            actual.len(),
            traded,
            "the board must hold exactly the contracts that traded"
        );
        assert_eq!(
            actual, expected,
            "the work-list sweep and a full-map walk must produce the same board"
        );
    }

    /// The roll is per CADENCE. Rolling one cadence's baseline must not
    /// disturb ANY other — they measure different windows and share only the
    /// contract.
    ///
    /// Driven from `SnapshotCadence::ALL`, not from a named 1s/5s pair: with
    /// two cadences a pairwise test was the whole space, and with four it is
    /// one sixth of it. The shape this catches is an off-by-one in the slot
    /// arithmetic, which a test of slots 0 and 2 alone can easily miss.
    #[test]
    fn roll_baselines_for_one_cadence_leaves_every_other_alone() {
        for rolled in SnapshotCadence::ALL {
            let mut lb = VolumeLeaderboard::new();
            lb.observe(stock(1, 100, 1_000), OptionFamily::Stock);
            lb.observe(stock(1, 100, 10_000), OptionFamily::Stock);

            lb.roll_baselines(OptionFamily::Stock, rolled);
            lb.observe(stock(1, 100, 10_400), OptionFamily::Stock);

            // The rolled cadence measures only what traded since ITS roll.
            let after = lb.rank(OptionFamily::Stock, rolled, 10, lot1, all);
            assert_eq!(
                after[0].window_lots_milli,
                400_000,
                "{}: the rolled baseline must measure only the 400 units since \
                 the roll",
                rolled.as_str()
            );

            // Every OTHER cadence still measures from its own seed. Ranking
            // one rolls it, so each is read on a freshly built board to keep
            // the cadences independent of the order this loop reads them in.
            for other in SnapshotCadence::ALL {
                if other == rolled {
                    continue;
                }
                let mut lb2 = VolumeLeaderboard::new();
                lb2.observe(stock(1, 100, 1_000), OptionFamily::Stock);
                lb2.observe(stock(1, 100, 10_000), OptionFamily::Stock);
                lb2.roll_baselines(OptionFamily::Stock, rolled);
                lb2.observe(stock(1, 100, 10_400), OptionFamily::Stock);
                let untouched = lb2.rank(OptionFamily::Stock, other, 10, lot1, all);
                assert_eq!(
                    untouched[0].window_lots_milli,
                    9_400_000,
                    "rolling {} disturbed {}: an unrolled baseline must still \
                     measure from its own seed — 10,400 - 1,000",
                    rolled.as_str(),
                    other.as_str()
                );
            }
        }
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

    /// Drive `id` to a re-latch and return the low value it adopted.
    ///
    /// Mirrors the production shape exactly: a high is latched, the vendor's
    /// counter then reads low for `RELATCH_AFTER_CONSECUTIVE_LOWER`
    /// consecutive observations, and the last of those abandons the high.
    fn relatch_to(
        lb: &mut VolumeLeaderboard,
        id: u64,
        underlying: u64,
        high: u32,
        low: u32,
    ) -> u32 {
        lb.observe(stock(id, underlying, high), OptionFamily::Stock);
        for _ in 0..RELATCH_AFTER_CONSECUTIVE_LOWER {
            let _ = lb.observe(stock(id, underlying, low), OptionFamily::Stock);
        }
        assert_eq!(lb.relatches(OptionFamily::Stock) > 0, true);
        low
    }

    #[test]
    fn a_recovery_to_the_abandoned_high_is_absorbed_not_ranked() {
        // THE PHANTOM. A re-latch reseeds every baseline to the LOW value it
        // adopted. If the vendor's counter was merely DIPPING and returns to
        // its true reading, the advance back to that reading would otherwise
        // compute `high - low` on all four cadences at once -- a
        // full-session-sized delta from a move that never happened, ranking
        // this contract first on every board and handing it a depth-200
        // socket it did not earn.
        //
        // BITE: delete the resync arm in `observe` and this test fails on the
        // `window_lots_milli` assertion with 999,000 * LOTS_SCALE.
        let mut lb = VolumeLeaderboard::new();
        relatch_to(&mut lb, 1, 100, 1_000_000, 1_000);
        // A second contract that genuinely trades, so "did not rank" is a
        // comparison rather than an empty board.
        lb.observe(stock(2, 200, 5_000), OptionFamily::Stock);
        let _ = lb.rank(OptionFamily::Stock, S1, 10, lot1, all);

        // The dip ends: the vendor reports the true cumulative again.
        assert_eq!(
            lb.observe(stock(1, 100, 1_000_000), OptionFamily::Stock),
            Observation::ResyncedToCeiling {
                ceiling: 1_000_000,
                adopted: 1_000_000,
            },
            "the climb back to the abandoned high must be ABSORBED"
        );
        assert_eq!(lb.resyncs(OptionFamily::Stock), 1);

        lb.observe(stock(2, 200, 6_000), OptionFamily::Stock);
        let ranked = lb.rank(OptionFamily::Stock, S1, 10, lot1, all);
        assert_eq!(
            ranked[0].security_id, 2,
            "a contract that recovered 999,000 phantom units must NOT outrank \
             one that genuinely traded 1,000"
        );
    }

    #[test]
    fn volume_above_the_abandoned_high_is_credited_not_swallowed() {
        // The excess ABOVE the ceiling is volume that really did trade at the
        // vendor while we were looking at the dip. Reseeding to the CEILING
        // rather than to `contract.volume` is what credits it -- swallowing it
        // would under-report a contract that was genuinely busy.
        let mut lb = VolumeLeaderboard::new();
        relatch_to(&mut lb, 1, 100, 1_000_000, 1_000);
        let _ = lb.rank(OptionFamily::Stock, S1, 10, lot1, all);

        // Recovers to the ceiling PLUS 7,500 genuinely traded units.
        assert!(matches!(
            lb.observe(stock(1, 100, 1_007_500), OptionFamily::Stock),
            Observation::ResyncedToCeiling { .. }
        ));
        let ranked = lb.rank(OptionFamily::Stock, S1, 10, lot1, all);
        assert_eq!(
            ranked[0].window_lots_milli,
            7_500 * LOTS_SCALE,
            "the excess above the ceiling is real trading and must be ranked; \
             only the recovery TO the ceiling is absorbed"
        );
    }

    #[test]
    fn the_resync_disarms_so_a_later_climb_ranks_normally() {
        // ONE ceiling, ONE consumption. A contract that recovers and then
        // keeps trading must rank on that later trading like any other -- the
        // ceiling is not a permanent discount.
        let mut lb = VolumeLeaderboard::new();
        relatch_to(&mut lb, 1, 100, 1_000_000, 1_000);
        let _ = lb.rank(OptionFamily::Stock, S1, 10, lot1, all);
        assert!(matches!(
            lb.observe(stock(1, 100, 1_000_000), OptionFamily::Stock),
            Observation::ResyncedToCeiling { .. }
        ));
        let _ = lb.rank(OptionFamily::Stock, S1, 10, lot1, all);

        // Ordinary trading from here.
        assert_eq!(
            lb.observe(stock(1, 100, 1_002_000), OptionFamily::Stock),
            Observation::Accepted,
            "the ceiling was consumed -- this is a normal advance, not a resync"
        );
        assert_eq!(lb.resyncs(OptionFamily::Stock), 1, "exactly one absorption");
        let ranked = lb.rank(OptionFamily::Stock, S1, 10, lot1, all);
        assert_eq!(
            ranked[0].window_lots_milli,
            2_000 * LOTS_SCALE,
            "post-resync trading ranks at face value"
        );
    }

    #[test]
    fn a_second_relatch_overwrites_the_ceiling_instead_of_keeping_the_older_one() {
        // Keeping an older, HIGHER ceiling would leave a trap: a legitimate
        // later climb would trip it and have a real window silently eaten.
        // The ceiling always describes the high THIS re-latch abandoned.
        let mut lb = VolumeLeaderboard::new();
        relatch_to(&mut lb, 1, 100, 1_000_000, 1_000);
        let _ = lb.rank(OptionFamily::Stock, S1, 10, lot1, all);

        // A SECOND re-latch, from a much lower high.
        for _ in 0..RELATCH_AFTER_CONSECUTIVE_LOWER {
            let _ = lb.observe(stock(1, 100, 50), OptionFamily::Stock);
        }
        assert_eq!(lb.relatches(OptionFamily::Stock), 2);
        let _ = lb.rank(OptionFamily::Stock, S1, 10, lot1, all);

        // The armed ceiling is now 1,000 -- the high the SECOND re-latch
        // abandoned -- not the stale 1,000,000.
        assert_eq!(
            lb.observe(stock(1, 100, 1_000), OptionFamily::Stock),
            Observation::ResyncedToCeiling {
                ceiling: 1_000,
                adopted: 1_000,
            },
            "the ceiling must track the LATEST abandoned high"
        );
    }

    #[test]
    fn a_resync_leaves_the_contract_rankable_without_a_duplicate_work_entry() {
        // The dirty mask and the per-cadence work lists are ONE structure. The
        // resync arm preserves the mask and pushes nothing, exactly as the
        // re-latch arm does -- clearing the mask without removing the key from
        // the lists would let the next advance push a duplicate, and the same
        // contract would appear twice on one board.
        let mut lb = VolumeLeaderboard::new();
        relatch_to(&mut lb, 1, 100, 1_000_000, 1_000);
        let _ = lb.rank(OptionFamily::Stock, S1, 10, lot1, all);
        assert!(matches!(
            lb.observe(stock(1, 100, 1_000_000), OptionFamily::Stock),
            Observation::ResyncedToCeiling { .. }
        ));
        let _ = lb.rank(OptionFamily::Stock, S1, 10, lot1, all);
        lb.observe(stock(1, 100, 1_004_000), OptionFamily::Stock);

        let ranked = lb.rank(OptionFamily::Stock, S1, usize::MAX, lot1, all);
        assert_eq!(
            ranked.iter().filter(|r| r.security_id == 1).count(),
            1,
            "a resynced contract must appear EXACTLY once on the board"
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
            delta_units: 0,
            lot_size: 0,
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

    /// Advance `traded` of the `contracts` contracts, then time ONLY the
    /// sweep. Returns `(per-sweep duration, contracts the sweep ranked)`.
    ///
    /// The second value exists so the caller can REFUSE a vacuous measurement.
    ///
    /// The advance loop is deliberately outside the timer: it is the per-tick
    /// path, gated by its own DHAT test, and folding it in would report a cost
    /// the sweep does not pay. What it does for the timer is leave the work
    /// list at the length the sweep must then walk — the variable this harness
    /// exists to sweep across.
    fn timed_sweep_round(
        lb: &mut VolumeLeaderboard,
        lots: &std::collections::HashMap<ContractKey, u32>,
        contracts: u64,
        traded: u64,
        round: u64,
        k: usize,
    ) -> (std::time::Duration, usize) {
        for step in 0..traded {
            // Spread the traded set across the population rather than taking a
            // prefix, so the sweep's map probes are scattered exactly as they
            // are in production.
            let id = (step * contracts / traded.max(1)) % contracts;
            let volume = base_volume(id) + (round as u32 + 1) * (1 + (id as u32 % 97));
            lb.observe(stock(id, id % 220, volume), OptionFamily::Stock);
        }
        let start = std::time::Instant::now();
        let ranked = lb.rank(
            OptionFamily::Stock,
            S1,
            k,
            |c| lots.get(&(c.security_id, c.segment)).copied(),
            all,
        );
        let len = ranked.len();
        std::hint::black_box(len);
        (start.elapsed(), len)
    }

    /// Deliberately NOT pre-sorted: a sort's cost depends on the input order,
    /// and a pre-sorted input measures the best case.
    fn base_volume(id: u64) -> u32 {
        ((id.wrapping_mul(2_654_435_761)) % 5_000_000) as u32 + 1
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
    ///
    /// # ⚠ CORRECTED 2026-09-12 — the figure it replaced was ALSO fiction
    ///
    /// The version of this harness that produced "900 us at the authorized
    /// ceiling" seeded each contract with ONE `observe` and then ranked fifty
    /// times without trading again. `observe` seeds `baseline = [volume; N]`,
    /// so every delta was zero, every contract hit the `lots == 0` `continue`,
    /// and all fifty rounds sorted an EMPTY vector. Its only assertion was
    /// `< 1s`, which an empty sort passes trivially.
    ///
    /// That 900 us was then quoted as a measurement in a rule file, a plan and
    /// this module's own `rank` doc — the shape this repository keeps
    /// recording, arriving in the harness whose whole job is to stop it. It is
    /// withdrawn; the numbers below come from a sweep that is now ASSERTED to
    /// have ranked a full board, and the shape it measures is a sweep over
    /// the work list rather than over the map.
    ///
    /// # What it measures now
    ///
    /// The per-sweep cost against the variable that actually drives it: how
    /// many contracts TRADED in the window. The worst case (every contract
    /// traded, so the work list is the universe) bounds it; the swept
    /// realistic fractions are what the dirty sets buy. The advance loop sits
    /// OUTSIDE the timer — it is the per-tick path, gated by DHAT elsewhere.
    ///
    /// The lot-size lookup is a real HASH PROBE, not a constant. Production
    /// resolves it through `global_contract_underlying_map().owner_of(..)` —
    /// a global load plus a probe, once per contract per sweep — and the
    /// harness that produced the withdrawn figure passed `lot1`, a closure
    /// returning `Some(1)`, which the optimiser can fold away entirely.
    #[test]
    #[ignore = "wall-clock measurement, not a gate"]
    fn rank_sweep_cost_at_the_authorized_ceiling() {
        // The measured 2026-08-22 stock-option universe.
        const CONTRACTS: u64 = 20_220;
        const DEPTH_20: usize = 250;
        const ROUNDS: u64 = 50;

        // A real lot-size map, probed once per contract per sweep. Production
        // resolves this through a global map lookup; a constant closure would
        // let the optimiser delete the probe the sweep actually pays for.
        let lots: std::collections::HashMap<ContractKey, u32> = (0..CONTRACTS)
            .map(|id| ((id, ExchangeSegment::NseFno), 1 + (id as u32 % 4) * 25))
            .collect();

        let mut lb = VolumeLeaderboard::new();
        for id in 0..CONTRACTS {
            lb.observe(stock(id, id % 220, base_volume(id)), OptionFamily::Stock);
        }
        assert_eq!(lb.tracked(OptionFamily::Stock), CONTRACTS as usize);

        // Warm, so the first-call map/vec growth is not in the number.
        let _ = timed_sweep_round(&mut lb, &lots, CONTRACTS, CONTRACTS, 0, DEPTH_20);

        // WORST CASE: every contract in the universe traded in the window, so
        // the work list IS the universe and the sweep degenerates to the walk
        // the dirty sets exist to avoid.
        let mut worst = std::time::Duration::ZERO;
        let mut worst_ranked = 0usize;
        for round in 1..=ROUNDS {
            let (elapsed, ranked) =
                timed_sweep_round(&mut lb, &lots, CONTRACTS, CONTRACTS, round, DEPTH_20);
            worst += elapsed;
            worst_ranked = ranked;
        }
        let per_worst = worst / u32::try_from(ROUNDS).unwrap_or(u32::MAX);

        // REALISTIC: a 1-second window on ~20,000 strikes. The traded count is
        // ASSUMED — nobody has measured it, and the query that would is named
        // in `top_volume_rank_persistence`'s header. It is swept here rather
        // than asserted, so the reader sees the shape instead of one number.
        let mut realistic = Vec::new();
        for traded in [100u64, 500, 2_000] {
            let mut total = std::time::Duration::ZERO;
            let mut ranked_len = 0usize;
            for round in 1..=ROUNDS {
                let (elapsed, ranked) =
                    timed_sweep_round(&mut lb, &lots, CONTRACTS, traded, ROUNDS + round, DEPTH_20);
                total += elapsed;
                ranked_len = ranked;
            }
            realistic.push((
                traded,
                total / u32::try_from(ROUNDS).unwrap_or(u32::MAX),
                ranked_len,
            ));
        }

        // The depth-200 path: ONE full-population sort (k = usize::MAX) and
        // the greedy distinct pass over it — the production 5-second arm shape.
        let mut distinct = std::time::Duration::ZERO;
        let mut distinct_len = 0usize;
        for round in 1..=ROUNDS {
            for step in 0..CONTRACTS {
                let volume = base_volume(step) + (300 + round as u32) * (1 + (step as u32 % 97));
                lb.observe(stock(step, step % 220, volume), OptionFamily::Stock);
            }
            let start = std::time::Instant::now();
            // Inlined rather than `distinct_over_full_rank`, which passes the
            // constant `lot1` stub — the same probe the sweeps above pay.
            let ordered = lb.rank(
                OptionFamily::Stock,
                S1,
                usize::MAX,
                |c| lots.get(&(c.security_id, c.segment)).copied(),
                all,
            );
            let five = distinct_underlying_over(ordered, 5);
            distinct += start.elapsed();
            distinct_len = five.len();
            std::hint::black_box(distinct_len);
        }
        let per_distinct = distinct / u32::try_from(ROUNDS).unwrap_or(u32::MAX);

        // ONE `println!`, not one per row. `crates/app/src/lib.rs` carries an
        // UNCONDITIONAL `#![deny(clippy::print_stdout)]`, and `#[cfg(test)]`
        // code escapes it only because neither the CI clippy job nor the
        // documented `make` target passes `--all-targets`. That is a repo-wide
        // pre-existing condition (the sibling harnesses in `multi_tf_aggregator`
        // and `tick_gap_detector` print the same way), not something to fix
        // here — but a harness that quietly quadrupled the count would make it
        // worse for whoever does. The report is assembled, then printed once.
        let duty = per_worst.as_secs_f64() * 100.0;
        let duty5 = per_distinct.as_secs_f64() / 5.0 * 100.0;
        let mut report = format!(
            "MEASURED at {CONTRACTS} tracked contracts:\n  \
             every contract traded   rank(top {DEPTH_20}) = {per_worst:?} \
             -> {duty:.4}% duty at 1s   (ranked {worst_ranked})"
        );
        for (traded, per, len) in &realistic {
            let d = per.as_secs_f64() * 100.0;
            report.push_str(&format!(
                "\n  {traded:>5} traded           rank(top {DEPTH_20}) = {per:?} \
                 -> {d:.4}% duty at 1s   (ranked {len})"
            ));
        }
        report.push_str(&format!(
            "\n  every contract traded   full rank + distinct(5) = {per_distinct:?} \
             -> {duty5:.4}% duty at 5s  (distinct {distinct_len})"
        ));
        println!("{report}");

        // ⚠ THE ANTI-VACUITY GATE, and it is the reason this harness was
        // rewritten on 2026-09-12.
        //
        // Until then it seeded each contract with ONE `observe`, which sets
        // `baseline = [volume; N]`, and then ranked 50 times without trading
        // again. Every delta was 0, every contract hit the `lots == 0`
        // `continue`, and all fifty rounds sorted an EMPTY vector. The only
        // assertion was `per_sweep < 1s`, which an empty sort passes trivially
        // — so the "900 us at the authorized ceiling" figure this repository
        // quoted in a rule file, a plan and a module header was measuring
        // nothing at all. The dirty sets would have made it vacuous a second
        // way (rounds 2..50 draining an empty work list), which is how it was
        // found.
        assert_eq!(
            worst_ranked, DEPTH_20,
            "the sweep must actually rank a full board, or this harness is timing an empty \
             sort and its number is fiction"
        );
        for (traded, _, len) in &realistic {
            assert!(
                *len > 0,
                "the {traded}-traded round ranked nothing — a vacuous measurement"
            );
        }
        assert_eq!(
            distinct_len, 5,
            "the distinct pass must fill its five slots"
        );

        // Not a gate on the number -- a gate on the SHAPE. A sweep that took a
        // whole cadence would starve the drain arm it shares a task with, and
        // the tightest cadence is now 1 second, not 5.
        assert!(
            per_worst < std::time::Duration::from_millis(200),
            "a sweep must not approach the 1s cadence: {per_worst:?}"
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
        // Identity sweep row 22 (2026-09-08): finite but impossible. A
        // previous close of one paisa against a 240-rupee print is a corrupt
        // input, and it used to persist as +2,400,000%.
        assert_eq!(
            eligible_gain_pct(240.0, 0.01),
            None,
            "a corrupt previous close"
        );
        let crash = eligible_gain_pct(0.01, 240.0)
            .expect("a real crash to one paisa is inside the bound and is kept");
        assert!(
            (crash - (-99.995_833_333_333_33)).abs() < 1e-9,
            "a crash is a large NEGATIVE move, never refused for size alone: {crash}"
        );
        assert_eq!(
            eligible_gain_pct(100.0 + MAX_PLAUSIBLE_GAIN_PCT, 100.0),
            Some(MAX_PLAUSIBLE_GAIN_PCT),
            "the bound itself is admitted"
        );
        assert_eq!(
            eligible_gain_pct(100.0 + MAX_PLAUSIBLE_GAIN_PCT + 1.0, 100.0),
            None,
            "one percent past the bound is refused"
        );
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
                delta_units: 0,
                lot_size: 0,
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
                delta_units: 0,
                lot_size: 0,
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

    /// `rank` records the two numbers it actually divided, not a re-derivation.
    ///
    /// The persisted row exists so the ordering is "checkable from the table
    /// alone". That only holds if `delta_units` and `lot_size` are the values
    /// the division consumed — a second lookup could return a lot size that
    /// has since changed and produce a row whose own arithmetic does not close.
    #[test]
    fn rank_records_the_numerator_and_denominator_it_divided() {
        // `lot_of_fixture` gives security_id 1 a 200-unit lot — the operator's
        // own worked example, so the numbers below are his: 3,200 units traded
        // in the window against a 200-unit lot.
        let mut lb = VolumeLeaderboard::new();
        lb.observe(stock(1, 100, 50_000), OptionFamily::Stock);
        let opening = lb.rank(OptionFamily::Stock, S1, 10, lot_of_fixture, all);
        assert!(
            opening.is_empty(),
            "the first sweep sets the baseline and ranks nothing"
        );

        lb.observe(stock(1, 100, 50_000 + 3_200), OptionFamily::Stock);
        let ranked = lb.rank(OptionFamily::Stock, S1, 10, lot_of_fixture, all);

        assert_eq!(ranked.len(), 1);
        let row = ranked[0];
        assert_eq!(row.delta_units, 3_200, "units traded INSIDE the window");
        assert_eq!(row.lot_size, 200, "the denominator the division used");
        assert_eq!(row.volume, 53_200, "cumulative, and NOT the numerator");
        assert_eq!(row.window_lots_milli, 16 * LOTS_SCALE, "16 lots");

        // The trio closes: the two recorded inputs reproduce the recorded key.
        assert_eq!(
            u64::from(row.delta_units) * LOTS_SCALE / u64::from(row.lot_size),
            row.window_lots_milli
        );

        // And the operator's percentage falls straight out of it: 1500%.
        assert!(
            ((row.window_lots_milli as f64 / 10.0 - 100.0) - 1_500.0).abs() < f64::EPSILON,
            "3,200 units on a 200-unit lot is a +1500% change from one lot"
        );
    }

    /// A contract observed but never ranked still gets its baseline rolled, so
    /// `delta_units` can never accumulate several windows into one.
    #[test]
    fn delta_units_measures_one_window_even_after_a_sweep_that_ranked_nothing() {
        let mut lb = VolumeLeaderboard::new();
        lb.observe(stock(1, 100, 1_000), OptionFamily::Stock);
        let _ = lb.rank(OptionFamily::Stock, S1, 10, lot_of_fixture, all);

        // Window A: 2,000 units. Ranked.
        lb.observe(stock(1, 100, 3_000), OptionFamily::Stock);
        let a = lb.rank(OptionFamily::Stock, S1, 10, lot_of_fixture, all);
        assert_eq!(a[0].delta_units, 2_000);

        // Window B: nothing traded — the contract leaves the board entirely.
        let b = lb.rank(OptionFamily::Stock, S1, 10, lot_of_fixture, all);
        assert!(b.is_empty(), "a zero-lot window is not 'top volume'");

        // Window C: 400 units. `delta_units` must be 400 — NOT 2,400, which is
        // what it would read if the quiet window had failed to roll forward.
        lb.observe(stock(1, 100, 3_400), OptionFamily::Stock);
        let c = lb.rank(OptionFamily::Stock, S1, 10, lot_of_fixture, all);
        assert_eq!(c[0].delta_units, 400);
        assert_eq!(c[0].volume, 3_400, "cumulative keeps climbing regardless");
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
    fn the_lot_size_the_tick_carried_is_used_and_the_sweep_never_probes_for_it() {
        // The drain populates `lot_size` from the `ContractOwner` it ALREADY
        // looks up on every tick, so the sweep must not go back to the global
        // map for it — that probe is ~40% of the ceiling sweep (1.78 ms
        // without it, 2.95 ms with it, at 20,220 traded).
        //
        // The closure PANICS rather than counting calls. A counter would still
        // pass if the probe merely moved somewhere else in the loop; a panic
        // is the only assertion that means "this is not called at all".
        let mut lb = VolumeLeaderboard::new();
        let carried = |volume: u32| RankedContract {
            lot_size: 200,
            ..stock(1, 100, volume)
        };
        lb.observe(carried(1), OptionFamily::Stock);
        let never = |_: &RankedContract| -> Option<u32> {
            panic!("the sweep probed for a lot size the tick had already carried")
        };
        let _ = lb.rank(OptionFamily::Stock, S1, 10, never, all);
        lb.observe(carried(20_001), OptionFamily::Stock);
        let ranked = lb.rank(OptionFamily::Stock, S1, 10, never, all);
        assert_eq!(ranked.len(), 1);
        // 20,000 units on a 200-unit lot is exactly 100 lots.
        assert_eq!(ranked[0].window_lots_milli, 100 * LOTS_SCALE);
        assert_eq!(
            ranked[0].lot_size, 200,
            "the persisted row must record the denominator the division used"
        );
    }

    #[test]
    fn a_tick_that_carried_no_lot_size_still_falls_back_to_the_probe() {
        // Zero means "nothing carried one", and the probe is the FALLBACK
        // rather than a skip — so a caller that does not populate the field
        // behaves exactly as it did before this became an input. Every other
        // test in this module rides that arm, which is why it is asserted
        // explicitly here rather than left as an assumption.
        let mut lb = VolumeLeaderboard::new();
        assert_eq!(
            stock(1, 100, 1).lot_size,
            0,
            "the fixture must carry none, or this test proves nothing"
        );
        lb.observe(stock(1, 100, 1), OptionFamily::Stock);
        let _ = lb.rank(OptionFamily::Stock, S1, 10, lot_of_fixture, all);
        lb.observe(stock(1, 100, 20_001), OptionFamily::Stock);
        let ranked = lb.rank(OptionFamily::Stock, S1, 10, lot_of_fixture, all);
        assert_eq!(ranked.len(), 1);
        assert_eq!(
            ranked[0].lot_size, 200,
            "supplied by the probe, because the tick carried nothing"
        );
        assert_eq!(ranked[0].window_lots_milli, 100 * LOTS_SCALE);
    }

    #[test]
    fn a_carried_lot_size_overrides_a_probe_that_would_disagree() {
        // The two sources must never BOTH be consulted for one division: the
        // persisted row records one denominator, and a row whose `lot_size`
        // disagreed with what was actually divided by would be a plausible
        // lie rather than a record. The carried value wins, and the row says
        // so.
        let mut lb = VolumeLeaderboard::new();
        let carried = |volume: u32| RankedContract {
            lot_size: 200,
            ..stock(1, 100, volume)
        };
        let disagrees = |_: &RankedContract| Some(50_u32);
        lb.observe(carried(1), OptionFamily::Stock);
        let _ = lb.rank(OptionFamily::Stock, S1, 10, disagrees, all);
        lb.observe(carried(20_001), OptionFamily::Stock);
        let ranked = lb.rank(OptionFamily::Stock, S1, 10, disagrees, all);
        assert_eq!(ranked[0].lot_size, 200, "the carried lot, not the probe's");
        assert_eq!(
            ranked[0].window_lots_milli,
            100 * LOTS_SCALE,
            "divided by 200; the probe's 50 would have given 400 lots"
        );
    }

    #[test]
    fn the_drain_carries_the_lot_size_it_already_probed_for() {
        // The whole saving depends on the CALL SITE populating the field —
        // the fallback above means a drain that stopped doing so would still
        // rank correctly, silently, while paying the probe 20,220 times a
        // sweep again. Nothing else in this module can see that regression,
        // so it is pinned here on the production source.
        let drain = include_str!("dhan_feed_stack.rs");
        assert!(
            drain.contains("lot_size: owner.lot_size,"),
            "observe_for_ranking must carry the lot size from the owner probe \
             it already pays for; without it the sweep re-probes the global \
             map once per contract per sweep"
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
    fn underlying_gain_pct_agrees_with_the_verdict_it_shares_inputs_with() {
        // THE property this function exists for: the persisted column and the
        // gainer FILTER read the same two inputs, so a row can never say "+5%"
        // and have been filtered out as not-a-gainer. Sign of the percentage
        // IS the verdict.
        for (spot_paise, close_rupees) in [
            (110_00_i64, 100.0_f64), // up
            (90_00, 100.0),          // down
            (100_00, 100.0),         // flat
        ] {
            let pct = underlying_gain_pct(Some(spot_paise), Some(close_rupees));
            let verdict = underlying_gainer_verdict(Some(spot_paise), Some(close_rupees));
            match verdict {
                GainerVerdict::Gainer => assert!(
                    pct.is_some_and(|p| p > 0.0),
                    "a gainer must carry a positive percent, got {pct:?}"
                ),
                GainerVerdict::NotGainer => assert!(
                    pct.is_some_and(|p| p <= 0.0),
                    "a non-gainer must never carry a positive percent, got {pct:?}"
                ),
                GainerVerdict::Unknown => assert_eq!(pct, None),
            }
        }
    }

    #[test]
    fn underlying_gain_pct_is_none_wherever_the_verdict_is_unknown() {
        // The two must refuse on exactly the same inputs, or the column would
        // carry a number for a row the filter could not judge.
        for (spot, close) in [
            (None, Some(100.0_f64)),
            (Some(110_00_i64), None),
            (Some(0), Some(100.0)),
            (Some(-1), Some(100.0)),
            (Some(110_00), Some(0.0)),
            (Some(110_00), Some(f64::NAN)),
        ] {
            assert_eq!(
                underlying_gainer_verdict(spot, close),
                GainerVerdict::Unknown
            );
            assert_eq!(
                underlying_gain_pct(spot, close),
                None,
                "spot={spot:?} close={close:?}"
            );
        }
    }

    #[test]
    fn underlying_gain_pct_computes_the_same_percent_the_rupee_form_would() {
        // Paise on both sides of the ratio; a ratio is scale-invariant, so the
        // answer must equal the rupee computation to the last bit.
        let paise = underlying_gain_pct(Some(112_35_i64), Some(100.0)).expect("finite");
        let rupees = eligible_gain_pct(112.35, 100.0).expect("finite");
        assert!(
            (paise - rupees).abs() < 1e-9,
            "paise {paise} vs rupees {rupees}"
        );
    }

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
