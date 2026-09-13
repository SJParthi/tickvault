//! The depth-20 NAME board: top 6 stock underlyings by absolute percentage
//! move, plus NIFTY and BANKNIFTY unconditionally (operator, 2026-09-11).
//!
//! Authorized by the "2026-09-11 (THIRD)" section of, and amended by the
//! "2026-09-11 (FOURTH)" section of,
//! `.claude/rules/project/websocket-connection-scope-lock.md`, which carries
//! the operator's verbatim words. Two of them decide the shape of this module:
//!
//! * *"just pcik the percnetage mvoe dude to fidn the top 7 that too startign
//!   pre oprn itself shodu lbe spotted"* — the key is a percentage MOVE of the
//!   UNDERLYING, and it must work before 09:15 where volume is zero for
//!   everything.
//! * *"firget the fucking dpeth 20 seconds level resuscoirbe … just go ehad
//!   wiht this newer mintue level resubscrbe"* — the apply cadence stays at one
//!   minute. That withdrawal is what makes this design shippable at all; the
//!   four blockers the 2026-09-09 section records against a 5-second cadence are
//!   moot here rather than deferred.
//!
//! # Why the key is INTEGER BASIS POINTS and not a percentage
//!
//! Not tidiness — the float path is a recorded defect class in this repository.
//! The 2026-09-07 lock bans a float sort key in as many words, because a
//! comparator that can return `NaN` is non-transitive and `sort_unstable_by`
//! then corrupts the slice WHOLESALE rather than misplacing one entry. And
//! `NaN` is not hypothetical here: `parser/quote.rs` carries a test asserting
//! the wire emits a `NaN` `day_close`, and Ticker mode emits `0.0`.
//!
//! Both inputs are already integers — `SpotPriceStore` holds paise and
//! `PrevCloseStore`'s rupees convert through `rupees_to_paise` — so integer
//! basis points are not a workaround, they are the natural representation, and
//! they remove the failure mode instead of guarding against it.
//!
//! # Why the arithmetic is `i128` and not `i64`
//!
//! `overflow-checks = true` and `panic = "abort"` are both set on the release
//! profile (CLAUDE.md, CARGO). So `delta_paise * 10_000` overflowing `i64` is
//! not a wrapped number — it is **immediate process death**, and this runs on
//! the frame drain, the one task emptying the socket.
//!
//! `SpotPriceStore` bounds paise only at `2^53`, so a corrupt `f32` decoding to
//! a large price passes every upstream gate and still reaches here. `2^53 ×
//! 10_000 ≈ 9.0e19`, which is past `i64::MAX ≈ 9.2e18`. The widening to `i128`
//! makes the multiply unconditionally safe, and the result is bounded back into
//! `i64` by the plausibility gate below before it is ever used as a key.
//!
//! # Why there is a plausibility ceiling
//!
//! `eligible_gain_pct`'s own doc records why one is needed: *"a previous close
//! of 0.01 against a 240-rupee LTP yields +2,400,000% — finite, sortable, and
//! nonsense"*. Finite and sortable is exactly what makes it dangerous — it
//! would pin rank 1 for the whole session, which is the threshold-poisoning
//! shape `VOLUME-MONO-01` exists for on the volume board.
//!
//! # What this module does NOT do
//!
//! It ranks NAMES. It does not expand a name into contracts, does not talk to
//! a socket, and does not decide swaps — those are the caller's, and keeping
//! them out is what makes every function here a pure function with a real test.

use tickvault_common::types::ExchangeSegment;
use tickvault_core::websocket::pool_supervisor::SubscribeInstrument;

use crate::depth_rebalance::MoverRow;
use crate::depth200_candidates::NOT_YET_RANKED;
use crate::dhan_contract_universe::ContractRow;
use crate::dhan_depth_universe::DepthCandidate;

/// How many STOCK underlyings enter the board.
///
/// The operator's number, verbatim: *"top 6 alone dude"* (2026-09-11 FOURTH).
/// It was 7 until the index window widened to ±11.
///
/// **Six is FORCED, not chosen.** Two independent pieces of arithmetic land on
/// it: the budget (`2 × 47 + 7 × 24 = 262` against 250), and the fact that the
/// freed index slots can buy neither a wider stock ladder (`2 × 47 + 6 × 28 =
/// 262`) nor a seventh name. Spot is the only thing that fits the room ±11
/// creates.
pub const DEPTH20_NAME_ENTRY_RANK: usize = 6;

/// How far a held name may slip before it loses its slots.
///
/// A held name is KEPT while its rank is `<= DEPTH20_NAME_EXIT_RANK`, so it
/// must fall out of the top 12 (~6% of the ~208 live F&O underlyings) before
/// its 24 contracts are given away.
///
/// **This band is the remedy the 2026-09-07 lock prescribes in advance**, not
/// an invention: *"If the swap budget is hit routinely, the answer is a longer
/// window or a hysteresis band on entry/exit"*. It is needed because a
/// percentage-move key re-orders in BOTH directions every minute, unlike
/// cumulative volume which only ever rises — and moving ONE name costs 23 of
/// the 20 swaps a minute affords, so a board that re-ordered freely could never
/// catch up to its own ranking. (The band is unchanged at 12 by the
/// 2026-09-11 FOURTH amendment; only the entry set narrowed, so the band is
/// now six ranks deep rather than five.)
///
/// **NOT derived.** No measurement of minute-to-minute rank drift on this key
/// exists, because no session has ever ranked on it. 12 is the first value with
/// a defensible MEANING — *entered as one of the seven most-moved names, keeps
/// its slots until it is no longer in the top dozen*. The read-out that tunes
/// it is the swap counter, and it must move before this number does.
pub const DEPTH20_NAME_EXIT_RANK: usize = 12;

/// Strikes each side of at-the-money for a STOCK name.
///
/// **This is an arithmetic CEILING, not a preference.** The board is
/// `2 × slots_for_index_name(11) + 6 × slots_for_stock_name(N)` against a hard
/// 250. At `N = 5` that is 238; at `N = 6` it is 262, and `plan_pool` refuses
/// the WHOLE pool fail-closed rather than truncating — a session-ending
/// failure, not a degraded one.
pub const DEPTH20_STOCK_ATM_STRIKES_EACH_SIDE: usize = 5;

/// Strikes each side of at-the-money for NIFTY and BANKNIFTY.
///
/// The operator's number, verbatim: *"instea dof atm plus or minus 10 can we go
/// ahead with plus or minus 11"* (2026-09-11 FOURTH). It was 10, quoted from
/// the THIRD authorization, until he spent the wasted index slots.
///
/// **±11 is the SOCKET ceiling.** An index name is
/// `slots_for_index_name(N)` and one depth-20 connection admits
/// [`DEPTH20_PER_SOCKET`]: ±11 is 47 of 50, and ±12 is 51 — over on its own,
/// before the rest of the board is even counted.
pub const DEPTH20_INDEX_ATM_STRIKES_EACH_SIDE: usize = 11;

/// The depth-20 instrument budget: 5 sockets × 50.
pub const DEPTH20_INSTRUMENT_BUDGET: usize = 250;

/// Rank on the ABSOLUTE move, so a faller ranks with a riser.
///
/// **This is the one place this module ASSUMES rather than quotes.** The
/// 2026-09-11 authorization says *"percnetage mvoe"* and *"percnetage change"*
/// without a direction, and the operator's 2026-09-06 words were *"top 7 among
/// between top gainers losers combined"* — so absolute is the reading that
/// honours both. It is a single constant precisely so it is one line to flip if
/// he means gainers only, and the rule file records the assumption as an
/// assumption.
pub const DEPTH20_RANK_ABSOLUTE_MOVE: bool = true;

/// The largest move this board will rank, in basis points (1,000% = 100,000 bp).
///
/// Deliberately the same magnitude as `volume_leaderboard::MAX_PLAUSIBLE_GAIN_PCT`
/// (1,000%), expressed in this module's own unit. A move past it is a corrupt
/// previous close, not a stock — and admitting one would hand it rank 1 for the
/// session.
pub const MAX_PLAUSIBLE_MOVE_BPS: i64 = 100_000;

/// The largest FALL this board will rank, in basis points (−50% = −5,000 bp).
///
/// **A separate floor exists because the ceiling above can NEVER fire on a
/// fall — not rarely, never.** A spot cannot go below zero, so the most
/// negative move arithmetically expressible is −10,000 bp (−100%), and
/// [`MAX_PLAUSIBLE_MOVE_BPS`] is 100,000. The entire downside range therefore
/// fits under the ceiling by a factor of ten, and with
/// [`DEPTH20_RANK_ABSOLUTE_MOVE`] on, a spot of one paise against a ₹1,000
/// previous close reads as **9,999 bp and takes rank 1 for the minute**. The
/// asymmetry is structural: an up-move is unbounded, so a ceiling bounds it; a
/// down-move is already bounded, so only a floor bounds it.
///
/// **−50% is not a guess at "corrupt".** It is the point past which the
/// reading is not a price move at all:
///
/// * a 1:2 split or a 1:1 bonus prints the spot at half the un-adjusted
///   previous close — a legitimate, RECURRING −50% that is not a move, and
///   would hand the ex-date name rank 1 every time;
/// * a 1:10 split prints −90%;
/// * NSE halts the whole market on a 20% index fall, and individual F&O
///   underlyings trade inside an exchange-set operating range, so a real
///   −50% session in an underlying is not a state the market stays open in.
///
/// The floor cannot separate a corporate action from a corrupt tick by
/// magnitude — and does not need to. In both cases the name must not rank,
/// because in both cases the number is not a move.
pub const MIN_PLAUSIBLE_MOVE_BPS: i64 = -5_000;

/// Basis points per unit ratio.
const BPS_SCALE: i128 = 10_000;

/// The floor must be TIGHTER than the mathematical minimum, or it is dead
/// code in exactly the way the ceiling was.
///
/// A spot of zero is already refused, so the most negative move reachable is
/// `-BPS_SCALE` (−100%). A floor at or below that can never fire — which is
/// the whole defect this constant was added to fix, so it is made
/// unrepresentable rather than left to a comment.
const _: () = assert!(
    MIN_PLAUSIBLE_MOVE_BPS > -(BPS_SCALE as i64),
    "a floor at or below -100% can never fire: the spot is already gated positive"
);
const _: () = assert!(
    MIN_PLAUSIBLE_MOVE_BPS < 0 && MAX_PLAUSIBLE_MOVE_BPS > 0,
    "the band must straddle a flat move, or a flat name is unrankable"
);

/// The absolute move of one underlying, in integer basis points.
///
/// # ⚠ ZERO PRODUCTION CALLERS (recorded 2026-09-13) — read this first
///
/// `grep -rn 'move_bps\b' crates/*/src` finds this function's own definition,
/// the [`NameMove`] field of the same name, the comparator in [`rank_names`]
/// that orders that field, and these doc links. **It finds no call site.**
/// The key the board actually ranks on is produced by
/// [`move_bps_from_pct`], from a QuestDB `close_pct_from_prev_day` column.
///
/// It cannot be wired today, and the reason is structural rather than a
/// missing line: its two inputs live in `SpotPriceStore` and `PrevCloseStore`,
/// both owned `&mut` by the frame drain. The steering loop that plans this
/// board has no handle to either and cannot be given one without putting a
/// lock on the hot path. `move_bps_from_pct`'s own docstring carries the full
/// substitution table — same question, different source, a minute of lag.
///
/// It is kept rather than deleted because it is the TESTED reference for the
/// integer path the 2026-09-07 lock describes, and the arithmetic below is
/// what the float boundary in `move_bps_from_pct` is checked against
/// (`move_bps_from_pct_is_integer_basis_points_and_absolute` asserts the two
/// agree on the same move). **A reader must not mistake the overflow
/// reasoning below for a description of the shipped path.** Nothing in
/// production reaches this `i128` multiply; the live path's cast guard is the
/// `±1e6` float bound in [`move_bps_from_pct`], which is a different
/// argument about a different number.
///
/// `None` — and the caller must treat it as NOT RANKABLE, never as zero — when:
///
/// * the previous close is absent or non-positive (a zero close is a live
///   Ticker-mode sentinel, and dividing by it is the defect this signature
///   exists to make unrepresentable);
/// * the spot is absent or non-positive;
/// * the computed move is outside
///   `[MIN_PLAUSIBLE_MOVE_BPS, MAX_PLAUSIBLE_MOVE_BPS]` — checked on the
///   SIGNED move, before any `abs()`, because after the `abs()` the sign that
///   decides which bound applies is gone.
///
/// **Ranking a missing price as `0` would be the false-OK**: it ties a name
/// nobody has a price for with a name that is genuinely flat, and between
/// 09:00 and 09:07 that is most of the equity universe — which is precisely
/// when this board is supposed to be forming.
///
/// # Overflow
///
/// The multiply is `i128`. See the module header: on this release profile an
/// `i64` overflow here is an abort on the frame drain, not a wrong number.
#[must_use]
pub fn move_bps(spot_paise: Option<i64>, prev_close_paise: Option<i64>) -> Option<i64> {
    let spot = spot_paise?;
    let prev = prev_close_paise?;
    if spot <= 0 || prev <= 0 {
        return None;
    }
    let delta = i128::from(spot) - i128::from(prev);
    let signed_bps = (delta * BPS_SCALE) / i128::from(prev);
    // Gate the SIGNED move, BEFORE the abs. Order is the safety property here:
    // `abs()` first would discard the sign that decides which bound applies,
    // and a -9,999 bp corrupt tick would pass a ceiling of 100,000 and take
    // rank 1. See MIN_PLAUSIBLE_MOVE_BPS.
    if signed_bps > i128::from(MAX_PLAUSIBLE_MOVE_BPS)
        || signed_bps < i128::from(MIN_PLAUSIBLE_MOVE_BPS)
    {
        return None;
    }
    // Bounded back into i64 by the gate above, so the cast cannot truncate.
    let bps = if DEPTH20_RANK_ABSOLUTE_MOVE {
        signed_bps.abs()
    } else {
        signed_bps
    };
    i64::try_from(bps).ok()
}

/// One underlying's standing on the board.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NameMove {
    /// The UNDERLYING's `SecurityId` — never a contract's.
    pub underlying_id: u64,
    /// The underlying's segment. Half of the I-P1-11 identity: `NSE_EQ` for a
    /// stock, `IDX_I` for an index, and Dhan reuses one numeric id across both.
    pub segment: ExchangeSegment,
    /// The rank key: absolute move since the previous close, in basis points.
    pub move_bps: i64,
}

impl NameMove {
    /// The I-P1-11 composite identity.
    #[must_use]
    pub fn key(&self) -> (u64, u8) {
        (self.underlying_id, self.segment.binary_code())
    }
}

/// Orders names by move, descending, with a total and stable tie-break.
///
/// The tie-break is `(move desc, underlying_id asc, segment asc)` — the same
/// shape `volume_leaderboard::rank` uses, so the two boards cannot disagree
/// about what "stable" means.
///
/// **Ties matter more here than on the volume board**, and that is worth
/// stating: basis points collide far more often than lot counts do, so the
/// `security_id` tie-break stops being a rare fallback and becomes a
/// systematic low-id preference among equally-moved names. That is a real
/// property of this key, it is deterministic, and it is not hidden.
///
/// # Complexity
///
/// O(1) EXEMPT: **O(n log n)** compares, where `n` is the RANKABLE name count
/// — at most one entry per row [`name_moves`] kept, so at most
/// `movers.len()`. The movers query is a `LATEST ON` partition over the live
/// F&O underlyings (~210 measured 2026-08-21), which puts the real bound at
/// roughly `210 × log2(210) ≈ 1,600` compares. The two index names are NOT in
/// this count: they are unconditional under the 2026-09-11 lock and never
/// ranked.
///
/// **Zero allocation, and that is a property of the SIGNATURE rather than of
/// the body**: taking the `Vec` by value and returning it lets
/// `sort_unstable_by` work in place, so the whole rank is one move and no
/// copy. Once a minute on the steering task — a sort this size is free at that
/// cadence, and the shape is recorded because every other function in this
/// module declares one and a silent omission reads as "audited and O(1)".
#[must_use]
pub fn rank_names(mut names: Vec<NameMove>) -> Vec<NameMove> {
    names.sort_unstable_by(|a, b| {
        b.move_bps
            .cmp(&a.move_bps)
            .then_with(|| a.underlying_id.cmp(&b.underlying_id))
            .then_with(|| a.segment.binary_code().cmp(&b.segment.binary_code()))
    });
    names
}

/// What the board decided this minute.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NameBoard {
    /// The names that may CLAIM slots: the top [`DEPTH20_NAME_ENTRY_RANK`].
    pub entry: Vec<NameMove>,
    /// The names that may KEEP slots they already hold: the top
    /// [`DEPTH20_NAME_EXIT_RANK`], which contains `entry` as its prefix.
    pub band: Vec<NameMove>,
}

/// Splits a ranked list into the entry set and the wider keep band.
///
/// The band CONTAINS the entry set as its prefix, so a caller that asks "may
/// this held name stay?" and "may this name arrive?" of the same board can
/// never get an inconsistent pair.
#[must_use]
pub fn split_board(ranked: &[NameMove]) -> NameBoard {
    NameBoard {
        entry: ranked
            .iter()
            .take(DEPTH20_NAME_ENTRY_RANK)
            .copied()
            .collect(),
        band: ranked
            .iter()
            .take(DEPTH20_NAME_EXIT_RANK)
            .copied()
            .collect(),
    }
}

impl NameBoard {
    /// May a name this pool ALREADY holds keep its contracts?
    ///
    /// The hysteresis question. Answering it from the band rather than the
    /// entry set is the whole reason the band exists.
    #[must_use]
    pub fn keeps(&self, key: (u64, u8)) -> bool {
        self.band.iter().any(|n| n.key() == key)
    }

    /// May a name CLAIM contracts it does not hold?
    #[must_use]
    pub fn admits(&self, key: (u64, u8)) -> bool {
        self.entry.iter().any(|n| n.key() == key)
    }
}

/// Instruments one depth-20 connection admits, per Dhan.
///
/// *"you can subscribe upto 50 instruments in a single connection"* —
/// `docs/dhan-ref/04-full-market-depth-websocket.md`. Mirrors
/// `MAX_INSTRUMENTS_PER_TWENTY_DEPTH_CONNECTION`; kept here so the per-socket
/// ceiling below reads against a named number rather than a literal.
pub const DEPTH20_PER_SOCKET: usize = 50;

/// Slots one INDEX name consumes: its nearest-expiry future, plus both legs of
/// every strike in its window.
///
/// `1 + (2N+1) × 2`. Every unique F&O contract is one subscription — a future
/// costs exactly what an option costs, because the pool dedups on the I-P1-11
/// composite and instrument class is invisible to the subscribe path.
///
/// **There is no index SPOT term, and there never can be.** An index is
/// `IDX_I`, and `subscription_builder::validate_depth_segment` REFUSES every
/// segment but `NSE_EQ` and `NSE_FNO` — so a NIFTY spot cannot reach a depth
/// socket even if someone asked for it. That refusal is the vendor's own
/// contract (*"Only NSE Equity and Derivatives segments supported"*), not our
/// preference.
#[must_use]
pub const fn slots_for_index_name(strikes_each_side: usize) -> usize {
    1 + (2 * strikes_each_side + 1) * 2
}

/// Slots one STOCK name consumes: its `NSE_EQ` SPOT, its nearest-expiry
/// future, plus both legs of every strike in its window.
///
/// The spot term is the 2026-09-11 (FOURTH) grant — *"in that top 6 try ot add
/// its udnerlying spot also"*. It is admissible where an index spot is not
/// because a cash equity is `NSE_EQ`, which Dhan documents as supported and
/// uses as its own subscribe example.
///
/// **Honest value: it buys levels 6–20 and nothing below them.** The main feed
/// runs Full mode and `append_inline_depth` already persists 5 levels of every
/// equity book, so levels 1–5 arrive today at no cost. What makes the trade
/// worth taking is that the freed slots can buy nothing else: a wider stock
/// ladder is 262 and a seventh name is 262, both over the 250 budget.
#[must_use]
pub const fn slots_for_stock_name(strikes_each_side: usize) -> usize {
    1 + slots_for_index_name(strikes_each_side)
}

/// The whole board's slot cost: two index names plus the stock entry set.
#[must_use]
pub const fn board_slot_cost() -> usize {
    2 * slots_for_index_name(DEPTH20_INDEX_ATM_STRIKES_EACH_SIDE)
        + DEPTH20_NAME_ENTRY_RANK * slots_for_stock_name(DEPTH20_STOCK_ATM_STRIKES_EACH_SIDE)
}

/// The authorized shape must fit the socket budget — at compile time, because
/// discovering it at runtime means `plan_pool` refusing the WHOLE pool
/// fail-closed and a session with no depth at all.
const _: () = assert!(
    board_slot_cost() <= DEPTH20_INSTRUMENT_BUDGET,
    "the depth-20 name board must fit 250 instruments; widen the stock ATM \
     window and it does not — 238 at ±5, 262 at ±6"
);

/// An INDEX name must fit ONE socket, or its ladder is split across two
/// connections and no single line carries the whole book.
///
/// This is the ceiling that makes ±11 the operator's maximum rather than his
/// preference: 47 of 50 at ±11, 51 at ±12 — over before the board is counted.
const _: () = assert!(
    slots_for_index_name(DEPTH20_INDEX_ATM_STRIKES_EACH_SIDE) <= DEPTH20_PER_SOCKET,
    "an index name must fit one depth-20 socket: 47 at ±11, 51 at ±12"
);

/// The band must be strictly wider than the entry set, or there is no
/// hysteresis at all and a name loses 23 slots on one minute's slip.
const _: () = assert!(
    DEPTH20_NAME_EXIT_RANK > DEPTH20_NAME_ENTRY_RANK,
    "the keep band must be wider than the entry set"
);

/// Appends one instrument to a socket, if the socket has room and no earlier
/// socket already claimed it.
///
/// Private and taking `seen` by argument rather than being a closure over it:
/// a closure capturing `seen` mutably cannot coexist with the `&mut plan`
/// borrows around it, and threading the set through is what keeps ONE running
/// claim set across the whole layout instead of a per-socket one.
fn claim_instrument(
    out: &mut Vec<SubscribeInstrument>,
    instrument: SubscribeInstrument,
    seen: &mut std::collections::HashSet<(u64, u8)>,
) {
    if out.len() < DEPTH20_PER_SOCKET
        && instrument.security_id > 0
        && seen.insert((instrument.security_id, instrument.segment.binary_code()))
    {
        out.push(instrument);
    }
}

// ---------------------------------------------------------------------------
// THE WIRING (2026-09-13): from a ranked list of NAMES to a depth-20 LAYOUT.
// ---------------------------------------------------------------------------
//
// Everything above this line ranks names and answers hysteresis questions. It
// had ZERO production call sites for two days — the skeleton-PR shape
// `audit-findings-2026-04-17.md` Rule 14 forbids — because nothing turned a
// name into the contracts a socket can carry. This section is that turn.
//
// # Why it emits a [`Depth20Layout`] rather than its own plan type
//
// `depth20_track::plan_depth20_minute` already carries the discipline this
// board needs and must not re-derive: content-based socket pairing (never
// positional, because `plan_pool` re-shards), paired departures/arrivals so a
// connection can never exceed its 50, plan-wide dedup so one instrument is
// never subscribed twice (Dhan answers a duplicate with an 804, which is
// Fatal), and a deterministic order so a swap the guard refuses once is not
// refused forever. Reimplementing any of that to carry a different struct
// would be a second copy of the hardest code in this pool.
//
// # The socket shape, and why it is 2 + 3 rather than one name per socket
//
// Two index sockets whole, then the stock names FLAT-CHUNKED across the
// remaining three. The scope lock's 2026-09-11 (FOURTH) section refuses
// socket-affinity in as many words — *"a name confined to one socket draws on
// one 4-swap budget while a freely-packed name draws on several"* — so a name
// deliberately spills across a chunk boundary. The index halves are kept whole
// because that is also what the legacy layout does, which is what makes the
// overlap pairing unambiguous at the handover minute.

/// Sockets the stock names are packed across: the pool's five, less the two
/// the index names hold whole.
pub const DEPTH20_NAME_STOCK_SOCKETS: usize = 3;

/// The stock half must fit the sockets it is packed into, or the chunking
/// hands a connection more than it can carry and the excess dies as a
/// `channel_full` refusal nobody reads.
const _: () = assert!(
    DEPTH20_NAME_ENTRY_RANK * slots_for_stock_name(DEPTH20_STOCK_ATM_STRIKES_EACH_SIDE)
        <= DEPTH20_NAME_STOCK_SOCKETS * DEPTH20_PER_SOCKET,
    "the six stock names must fit three depth-20 sockets: 144 of 150 at ±5"
);

/// The board's two index names hold one socket each, so exactly two are
/// authorized. Asserted rather than assumed because [`board_slot_cost`]
/// multiplies by a literal 2.
const _: () = assert!(
    crate::depth20_layout::DEPTH_20_INDEX_UNDERLYINGS.len() + DEPTH20_NAME_STOCK_SOCKETS
        == crate::depth20_layout::DEPTH_20_SOCKETS,
    "two index sockets plus three stock sockets is the depth-20 pool"
);

/// The absolute move of one underlying, from a MOVERS ROW's percentage.
///
/// # ⚠ This is a SUBSTITUTION, and it is not the same source as [`move_bps`]
///
/// [`move_bps`] takes a live spot and a live previous close — the pair the
/// frame drain holds in `SpotPriceStore` and `PrevCloseStore`, and the pair
/// the drain's own gainer verdict is computed from. **That pair is not
/// reachable here.** `PrevCloseStore` is owned `&mut` by the drain; the
/// steering loop has no handle to it and cannot be given one without moving it
/// behind a lock on the hot path.
///
/// What the steering loop does have is [`MoverRow::pct_change`], which is
/// `close_pct_from_prev_day` read from QuestDB — the LATEST sealed `candles_1m`
/// row for the underlying, or, before the first candle seals at ~09:16, a
/// tick-derived pre-open ranking (`build_preopen_movers_query`). So it answers
/// the same QUESTION from a different source with a different freshness:
///
/// | | drain's gainer verdict | this |
/// |---|---|---|
/// | spot | last tick, in RAM | last SEALED minute (or a pre-open tick) |
/// | previous close | `PrevCloseStore`, in RAM | the candle's own column |
/// | lag | ~0 | up to one minute, plus the query |
/// | fails when | a price is missing | QuestDB is unreachable |
///
/// A minute of lag on a board that is re-planned once a minute is inside the
/// cadence, so the substitution is defensible — but it is a substitution, and
/// the two must never be described as one number. Recorded here rather than in
/// a commit message because the next reader will otherwise find `move_bps` and
/// this function side by side and assume they share an input.
///
/// # Why the float stops here
///
/// The 2026-09-07 lock bans a float in the sort key, and the ban is a defect
/// record rather than a preference: a comparator that can return `NaN` is
/// non-transitive and `sort_unstable_by` corrupts the slice WHOLESALE. So the
/// `f64` is converted, gated and discarded at this boundary; nothing downstream
/// of this function sees one. The band is [`move_bps`]'s own — same ceiling,
/// same floor, same gate-the-SIGNED-value-before-the-abs order, because a
/// corrupt previous close reaching the board through the candle column is the
/// same rank-1 poisoning as one reaching it through the drain.
#[must_use]
pub fn move_bps_from_pct(pct_change: f64) -> Option<i64> {
    if !pct_change.is_finite() {
        return None;
    }
    let scaled = (pct_change * 100.0).round();
    // Bound in FLOAT, against literals, so the cast below cannot truncate.
    // Deliberately far wider than the plausibility band: this guard is about
    // the CAST being exact, and the band below is about the number being a
    // price move. Conflating them would hide which one refused.
    if !(-1.0e6..=1.0e6).contains(&scaled) {
        return None;
    }
    #[expect(
        clippy::cast_possible_truncation,
        reason = "bounded to ±1e6 on the line above, and .round() made it integral"
    )]
    let signed_bps = scaled as i64;
    // Gate the SIGNED move BEFORE the abs — see MIN_PLAUSIBLE_MOVE_BPS. After
    // the abs the sign that decides which bound applies is gone, and a -99.99%
    // corrupt row would pass a ceiling of +100,000 and take rank 1.
    if !(MIN_PLAUSIBLE_MOVE_BPS..=MAX_PLAUSIBLE_MOVE_BPS).contains(&signed_bps) {
        return None;
    }
    Some(if DEPTH20_RANK_ABSOLUTE_MOVE {
        signed_bps.abs()
    } else {
        signed_bps
    })
}

/// Every rankable name in a movers slice, unranked.
///
/// A row whose move is not rankable is DROPPED, never carried at zero — the
/// whole point of [`move_bps_from_pct`] returning `Option`. Dropping is what
/// keeps an unpriced name out of the board instead of tying it with a
/// genuinely flat one.
///
/// # Complexity
///
/// O(1) EXEMPT: O(movers), bounded by the movers query's own `LATEST ON`
/// partition over the ~210 live F&O underlyings. Once a minute, steering task.
#[must_use]
pub fn name_moves(movers: &[MoverRow]) -> Vec<NameMove> {
    movers
        .iter()
        .filter(|row| row.security_id > 0)
        .filter_map(|row| {
            Some(NameMove {
                underlying_id: row.security_id,
                segment: row.segment,
                move_bps: move_bps_from_pct(row.pct_change)?,
            })
        })
        .collect()
}

/// Each underlying's nearest non-expired FUTURE, keyed by upper-cased symbol.
///
/// The board gives every name one future slot ([`slots_for_index_name`]'s
/// leading `1 +`). The contract artifact is the only authorized source: the
/// option chain carries no futures at all, and a hardcoded contract id expires
/// — the scope lock's standing REJECT.
///
/// Nearest expiry wins; the LOWEST id breaks a tie. Ties need a rule for the
/// same reason every other grouping here has one: the artifact carries no
/// `ORDER BY`, so last-write-wins would let two minutes of identical input
/// choose different contracts and swap the socket back and forth all session
/// while every counter read healthy.
///
/// A BSE future is refused by [`derivative_segment`] and an unsupported
/// segment by [`segment_supports_depth`] — SENSEX can never have a depth book,
/// so a SENSEX future here would be a socket that dies on connect.
///
/// # Complexity
///
/// O(1) EXEMPT: O(rows) over the day's contract artifact (~22,000 legs), of
/// which the class filter keeps the ~1,300 futures. Built ONCE per session by
/// the caller — the artifact is a daily file and `today_ymd` is fixed for the
/// session, so a per-minute rebuild would answer the same question 375 times.
#[must_use]
pub fn future_index(
    rows: &[ContractRow],
    today_ymd: u32,
) -> std::collections::HashMap<String, SubscribeInstrument> {
    let mut best: std::collections::HashMap<String, (u32, u64, ExchangeSegment)> =
        std::collections::HashMap::new();
    for row in rows {
        if !matches!(row.c.as_str(), "FUTSTK" | "FUTIDX") {
            continue;
        }
        // `>=` keeps expiry DAY itself, which is a trading day.
        if row.e < today_ymd || row.i == 0 {
            continue;
        }
        let Some(segment) = crate::dhan_contract_universe::derivative_segment(&row.x) else {
            continue;
        };
        if !crate::dhan_depth_universe::segment_supports_depth(segment) {
            continue;
        }
        let key = row.u.trim().to_ascii_uppercase();
        if key.is_empty() {
            continue;
        }
        match best.entry(key) {
            std::collections::hash_map::Entry::Occupied(mut held) => {
                if (row.e, row.i) < (held.get().0, held.get().1) {
                    held.insert((row.e, row.i, segment));
                }
            }
            std::collections::hash_map::Entry::Vacant(slot) => {
                slot.insert((row.e, row.i, segment));
            }
        }
    }
    best.into_iter()
        .map(|(symbol, (_, security_id, segment))| {
            (
                symbol,
                SubscribeInstrument {
                    security_id,
                    segment,
                },
            )
        })
        .collect()
}

/// The names this minute WANTS, in preference order — the hysteresis applied.
///
/// # Why this exists as its own function
///
/// [`NameBoard::keeps`] and [`NameBoard::admits`] are questions; nothing was
/// asking them. A band that is computed and then ignored is worse than no
/// band, because the swap counters then report controlled churn while the
/// board re-orders freely underneath — so the exit rank has to govern the
/// CHOICE of names, which is here, and not merely be available to a caller
/// that might consult it.
///
/// # ⚠ The tail is ENTRY-SET names only, and that is a real limit
///
/// The list is a PREFERENCE, not the answer: it is up to
/// [`DEPTH20_NAME_ENTRY_RANK`] incumbents followed by the entry set as
/// fallbacks, and the caller takes the first [`DEPTH20_NAME_ENTRY_RANK`] that
/// actually resolve to contracts. The tail exists so an INCUMBENT whose ladder
/// is missing this minute does not leave its slots empty when an entry-set
/// name could have them.
///
/// It does NOT extend past the entry set, so with no incumbents there is no
/// tail at all: if one of the top six cannot be resolved, the board carries
/// FIVE names and the sixth's slots stay empty for the minute. That is
/// deliberate — a band name may be KEPT and never PLACED, so promoting rank 7
/// into the hole would be the one route by which a name outside the top six
/// reaches a socket, and it would arrive through the error path rather than
/// through the ranking. `plan_depth20_minute` leaves an empty desired socket
/// completely alone, so the wire keeps what it has rather than being stripped
/// on the strength of a missing chain. Pinned by
/// `an_unresolvable_entry_name_leaves_its_slots_empty_rather_than_promoting_a_band_name`,
/// which was first written asserting the opposite and caught.
///
/// `held` is what the board CHOSE last minute, not what the wire acked. The
/// two differ while a swap is in flight — moving one name costs 24 swaps
/// against a budget of 20 a minute — and choosing from the wire would let a
/// name the board is still placing read as "not held" and be re-contested
/// every minute until it landed.
///
/// # Complexity
///
/// O(1) EXEMPT: O(EXIT_RANK × ENTRY_RANK) key compares in each pass, so at
/// most `12 × 6 + 6 × 12 = 144` in total — `DEPTH20_NAME_EXIT_RANK` is 12,
/// `DEPTH20_NAME_ENTRY_RANK` is 6, and `out` never exceeds the latter until
/// step 2. (144 was the documented figure before the step-1 self-dedup landed
/// on 2026-09-13, when the real bound was half of it; the dedup made the
/// stated number TRUE rather than raising it.) A linear scan of at most twelve
/// entries costs less than the `HashSet` that would replace it, and the
/// `BTreeSet` probe against `held` is the O(log) term that is already there.
/// Once a minute, steering task.
#[must_use]
pub fn choose_names(
    board: &NameBoard,
    held: &std::collections::BTreeSet<(u64, u8)>,
) -> Vec<NameMove> {
    let mut out: Vec<NameMove> = Vec::with_capacity(DEPTH20_NAME_EXIT_RANK);
    // 1. INCUMBENTS still inside the band, strongest first. This is the
    //    hysteresis: a name that entered at rank 6 keeps its slots at rank 11.
    //
    //    The `!out.iter().any(..)` is a SELF-dedup, and it is not symmetry
    //    with step 2 for its own sake. A duplicated `(security_id, segment)`
    //    in `movers` survives `name_moves` and `rank_names` as two entries, so
    //    the band can carry one name twice; without this test both copies are
    //    pushed. The second copy then reaches `build_name_layout`, resolves
    //    the SAME symbol, finds every one of its contracts already in `seen`,
    //    assembles an EMPTY `name_instruments` — and still runs
    //    `plan.chosen.push(name)`, burning one of six slots on a phantom that
    //    subscribes nothing. Not reachable today (`instrument_lifecycle`'s
    //    DEDUP keys make the movers join 1:1), which is exactly why it is
    //    cheap to make unrepresentable now rather than after a query change.
    for name in &board.band {
        if held.contains(&name.key())
            && out.len() < DEPTH20_NAME_ENTRY_RANK
            && !out.iter().any(|chosen| chosen.key() == name.key())
        {
            out.push(*name);
        }
    }
    // 2. Then the ENTRY set — for the slots the incumbents left, and as the
    //    fallback for an incumbent whose ladder does not resolve. A band-only
    //    name is deliberately absent: it may be KEPT, never PLACED.
    for name in &board.entry {
        if !out.iter().any(|held_name| held_name.key() == name.key()) {
            out.push(*name);
        }
    }
    out
}

/// One name's ±`each_side` strike window, both legs, ascending by strike.
///
/// `bucket` is the candidate rows for ONE underlying — the decorate-index the
/// caller builds in a single pass, which is what keeps this off the
/// `O(movers × candidates)` shape `atm_pair_for`'s docstring records.
///
/// Everything refused is refused for the same reasons the chain view refuses
/// it, and each refusal costs a strike rather than inventing one:
///
/// * rows outside the underlying's NEAREST expiry — a far-month contract at a
///   strike chosen from this month's spot quotes almost nothing, and nothing
///   downstream can tell that subscription from a real one;
/// * a strike carrying only one leg — the pair is the unit, and a stranded leg
///   means two sockets reading different books;
/// * a strike whose two legs share one id — malformed, and honouring it spends
///   two authorized slots on one instrument;
/// * a non-positive contract id — instrument 0 is a well-formed subscription
///   that returns silence forever and looks healthy.
///
/// It never PADS. A window short on one side is normal near the edge of a
/// freshly-listed expiry, and a strike that does not exist cannot be
/// subscribed.
///
/// # Complexity
///
/// O(1) EXEMPT: O(bucket) to group plus O(k log k) to order one underlying's
/// strikes — at most a few hundred. Called at most **14** times a minute on
/// the steering task, not eight: [`build_name_layout`] calls it once per
/// ITERATED name, and its own complexity section derives the 14 (2 index
/// names plus the up-to-12 preference list [`choose_names`] can return).
/// Corrected 2026-09-13 alongside the same stale figure there.
#[must_use]
pub fn strike_window(
    bucket: &[&DepthCandidate],
    spot: f64,
    each_side: usize,
    segment: ExchangeSegment,
) -> Vec<SubscribeInstrument> {
    if !spot.is_finite() || spot <= 0.0 {
        return Vec::new();
    }
    let Some(spot_paise) = crate::depth_rebalance::strike_paise(spot) else {
        return Vec::new();
    };
    // CURRENT EXPIRY ONLY. `None` — every row missing an expiry — refuses
    // nothing on that basis, matching `is_nearest_expiry`.
    let nearest = bucket
        .iter()
        .filter(|c| c.expiry_micros > 0)
        .map(|c| c.expiry_micros)
        .min();
    let mut legs: std::collections::HashMap<i64, (Option<i64>, Option<i64>)> =
        std::collections::HashMap::new();
    for candidate in bucket {
        if let Some(expiry) = nearest
            && candidate.expiry_micros != expiry
        {
            continue;
        }
        if candidate.contract_security_id <= 0 {
            continue;
        }
        let Some(paise) = crate::depth_rebalance::strike_paise(candidate.strike) else {
            continue;
        };
        let slot = legs.entry(paise).or_insert((None, None));
        // LOWEST id wins a contested leg, never the last row seen — the query
        // carries no `ORDER BY`, so last-wins makes the answer depend on
        // arrival order and the socket flips between two ids all session.
        match candidate.leg.as_str() {
            "CE" => {
                slot.0 = Some(slot.0.map_or(candidate.contract_security_id, |had| {
                    had.min(candidate.contract_security_id)
                }));
            }
            "PE" => {
                slot.1 = Some(slot.1.map_or(candidate.contract_security_id, |had| {
                    had.min(candidate.contract_security_id)
                }));
            }
            _ => {}
        }
    }
    let mut pairs: Vec<(i64, i64, i64)> = legs
        .iter()
        .filter_map(|(paise, (ce, pe))| {
            let (ce, pe) = ((*ce)?, (*pe)?);
            // One contract cannot be both legs of its own strike.
            if ce == pe {
                return None;
            }
            Some((*paise, ce, pe))
        })
        .collect();
    if pairs.is_empty() {
        return Vec::new();
    }
    pairs.sort_unstable_by_key(|(paise, _, _)| *paise);
    // The centre: nearest strike to spot, ties to the LOWER strike. `<` rather
    // than `<=` over an ascending list is what makes the tie deterministic, so
    // an exact midpoint does not flip the window between two strikes on
    // alternating minutes.
    let mut centre = 0usize;
    let mut best = i64::MAX;
    for (index, (paise, _, _)) in pairs.iter().enumerate() {
        let distance = (paise - spot_paise).abs();
        if distance < best {
            best = distance;
            centre = index;
        }
    }
    let lo = centre.saturating_sub(each_side);
    let hi = centre
        .saturating_add(each_side)
        .min(pairs.len().saturating_sub(1));
    let mut out = Vec::with_capacity((hi - lo + 1) * 2);
    for (_, ce, pe) in &pairs[lo..=hi] {
        for id in [*ce, *pe] {
            if let Ok(security_id) = u64::try_from(id)
                && security_id > 0
            {
                out.push(SubscribeInstrument {
                    security_id,
                    segment,
                });
            }
        }
    }
    out
}

/// What the name board decided this minute, and everything it could not place.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct NameBoardPlan {
    /// The five depth-20 sockets, in dial order: NIFTY, BANKNIFTY, then the
    /// stock names flat-chunked across three.
    pub layout: crate::depth20_layout::Depth20Layout,
    /// The STOCK names that actually took slots, in rank order. The caller
    /// carries this into the next minute as `held` — see [`choose_names`].
    pub chosen: Vec<NameMove>,
    /// Index underlyings whose window could not be resolved this minute.
    pub index_unresolved: Vec<String>,
    /// Preferred names whose option window could not be resolved.
    pub names_unresolved: usize,
    /// Names — index or stock — with no nearest-expiry future in the artifact.
    pub futures_missing: usize,
    /// Stock names whose `NSE_EQ` spot slot was refused.
    pub spots_missing: usize,
}

impl NameBoardPlan {
    /// Whether this plan may STEER, or whether the caller must fall back.
    ///
    /// Both conditions are load-bearing and neither is redundant:
    ///
    /// * **Both index windows resolved.** NIFTY and BANKNIFTY are
    ///   unconditional under the 2026-09-11 lock, so a plan missing one is not
    ///   the authorized board — and steering on it would strip a working index
    ///   socket on the strength of a chain that did not publish this minute.
    /// * **At least one stock name placed.** With none, this is the legacy
    ///   index layout at a different width: it would displace the seed hold
    ///   and the volume ranking for no gain, in exactly the pre-09:07 window
    ///   where ~750 equities have not printed at all.
    ///
    /// Failing either is the NORMAL pre-open state, not a fault. The caller
    /// keeps the engine it already had.
    #[must_use]
    pub fn is_steerable(&self) -> bool {
        self.index_unresolved.is_empty() && !self.chosen.is_empty()
    }

    /// The chosen stock names as I-P1-11 composite keys, for the next minute's
    /// hysteresis.
    #[must_use]
    pub fn chosen_keys(&self) -> std::collections::BTreeSet<(u64, u8)> {
        self.chosen.iter().map(NameMove::key).collect()
    }
}

/// One underlying's consensus spot — SKIPPED entirely when its bucket is empty.
///
/// [`strike_window`] genuinely needs a spot: it centres the window on the
/// nearest strike, so the price cannot be deferred for a name that HAS
/// strikes. But a name with no candidate rows at all has no strike pairs
/// either, so `strike_window` returns an empty window whatever spot it is
/// handed — while `crate::depth_rebalance::consensus_spot` would scan the
/// WHOLE candidate slice (~22,000 rows, measured 2026-09-12) to reach that
/// same answer.
///
/// **Behaviour-preserving by construction, not by inspection.** Two things
/// make the guard safe rather than merely plausible: `NaN` and the true price
/// produce the identical empty window when there are no pairs to centre on,
/// and `consensus_spot`'s only side effect — the `tally.len() > 1`
/// disagreement counter — cannot fire on a tally that would have stayed empty,
/// because every row it tallies is one this bucket does not contain.
///
/// It does NOT lower the worst-case bound in [`build_name_layout`]: a name
/// with rows but no resolvable PAIR still pays the scan. It removes the cost
/// of the common unresolvable case — a mover the contract artifact carries no
/// legs for at all, which is precisely the disagreement `names_unresolved`
/// exists to count.
fn spot_for_bucket(
    candidates: &[DepthCandidate],
    underlying: &str,
    bucket: &[&DepthCandidate],
) -> f64 {
    if bucket.is_empty() {
        return f64::NAN;
    }
    // CURRENT EXPIRY ONLY, matching `strike_window`'s own filter. `None` — a
    // bucket where every row is missing an expiry — refuses nothing.
    let nearest = bucket
        .iter()
        .filter(|c| c.expiry_micros > 0)
        .map(|c| c.expiry_micros)
        .min();
    crate::depth_rebalance::consensus_spot(candidates, underlying, nearest).unwrap_or(f64::NAN)
}

/// Builds the depth-20 layout from the NAME board.
///
/// The order of operations is the contract, and each step is where it is for a
/// reason a later reader should not have to rediscover:
///
/// 1. **Index names first, unconditionally.** NIFTY and BANKNIFTY are never
///    ranked and can never be displaced by a mover, so they claim their
///    instruments before any stock is considered — a stock that somehow shared
///    a contract id could then only lose it, never take it.
/// 2. **Then the hysteresis choice**, so an incumbent is preferred over a
///    higher-ranked newcomer.
/// 3. **Then resolution**, taking names until [`DEPTH20_NAME_ENTRY_RANK`]
///    RESOLVE — a name that cannot be resolved consumes no slot.
///
/// # Deduplication
///
/// One running set across the WHOLE layout, index sockets included. A
/// duplicate across two sockets spends two of the 250 authorized slots on one
/// book; a duplicate WITHIN one socket is worse, because both copies go out in
/// the same batch and Dhan answers a duplicate subscribe with an 804, which is
/// Fatal — the connection drops and does not come back this session.
///
/// # Complexity
///
/// O(1) EXEMPT: ONE O(candidates) bucketing pass, then O(bucket) per name
/// ITERATED, plus one O(candidates) `consensus_spot` scan per iterated name
/// whose bucket is non-empty.
///
/// **The iterated-name bound is 14, not 8** — corrected 2026-09-13, where this
/// paragraph said "at most eight" and understated its own worst case by
/// 1.75×. Derived from the constants in this file rather than counted from
/// the authorized board size:
///
/// * `DEPTH_20_INDEX_UNDERLYINGS.len()` = **2** index names, unconditional;
/// * [`choose_names`] returns at most `DEPTH20_NAME_ENTRY_RANK` incumbents
///   (step 1 is capped by `out.len() < DEPTH20_NAME_ENTRY_RANK`) followed by
///   at most `DEPTH20_NAME_ENTRY_RANK` entry names, so `6 + 6 = ` **12**;
/// * the loop's `break` fires on `plan.chosen.len() >= DEPTH20_NAME_ENTRY_RANK`
///   — on names that RESOLVED. A name that does not resolve `continue`s, so
///   the break caps the CHOSEN set at 6 and does not cap the iteration count
///   at all. Twelve consecutive unresolvable names is twelve iterations.
///
/// `2 + 12 = 14`. The eight this paragraph used to claim was the number of
/// names on a fully-resolved board (2 index + 6 stock), which is the BEST
/// case, not the worst.
///
/// So the honest shape is O(candidates × iterated) with `iterated <= 14`,
/// against the `O(movers × candidates)` ≈ 5M row-visit shape `atm_pair_for`'s
/// docstring records for the legacy layout — still two orders of magnitude
/// cheaper, which is why the correction changes the number and not the
/// verdict. `spot_for_bucket` removes the scan for an iterated name with NO
/// candidate rows, which is the common unresolvable case; it does not lower
/// the 14, because a name with rows but no resolvable strike PAIR still pays
/// it. Cold path, once a minute on the steering task.
#[must_use]
pub fn build_name_layout(
    candidates: &[DepthCandidate],
    movers: &[MoverRow],
    futures: &std::collections::HashMap<String, SubscribeInstrument>,
    held: &std::collections::BTreeSet<(u64, u8)>,
) -> NameBoardPlan {
    let mut plan = NameBoardPlan::default();
    let mut seen: std::collections::HashSet<(u64, u8)> = std::collections::HashSet::new();

    // The decorate-index: ONE pass, so no later step scans the whole slice
    // per name.
    let mut by_underlying: std::collections::HashMap<&str, Vec<&DepthCandidate>> =
        std::collections::HashMap::new();
    for candidate in candidates {
        by_underlying
            .entry(candidate.underlying.as_str())
            .or_default()
            .push(candidate);
    }

    // ---- 1. the two index names, whole sockets, unconditional ----
    for underlying in crate::depth20_layout::DEPTH_20_INDEX_UNDERLYINGS {
        let mut instruments: Vec<SubscribeInstrument> = Vec::with_capacity(DEPTH20_PER_SOCKET);
        match futures.get(underlying) {
            Some(future) => claim_instrument(&mut instruments, *future, &mut seen),
            None => plan.futures_missing = plan.futures_missing.saturating_add(1),
        }
        let bucket = by_underlying.get(underlying).map_or(&[][..], Vec::as_slice);
        let spot = spot_for_bucket(candidates, underlying, bucket);
        // Fail-CLOSED on an underlying we cannot name a contract segment for,
        // and on one whose segment the vendor refuses depth on. Guessing
        // subscribes a well-formed request for the wrong instrument, which
        // comes back as silence — indistinguishable from a quiet book.
        let options = crate::dhan_depth_universe::contract_segment_for_underlying(underlying)
            .filter(|segment| crate::dhan_depth_universe::segment_supports_depth(*segment))
            .map_or_else(Vec::new, |segment| {
                strike_window(bucket, spot, DEPTH20_INDEX_ATM_STRIKES_EACH_SIDE, segment)
            });
        if options.is_empty() {
            plan.index_unresolved.push((*underlying).to_owned());
        }
        for instrument in options {
            claim_instrument(&mut instruments, instrument, &mut seen);
        }
        // An index socket is emitted EVEN WHEN EMPTY: its POSITION is its
        // identity, and declining to emit it would slide a stock chunk into
        // index position, so the tracker would diff a stock ladder against
        // BANKNIFTY's held set and swap the whole socket.
        plan.layout
            .sockets
            .push(crate::depth20_layout::Depth20Socket {
                underlying: Some((*underlying).to_owned()),
                instruments,
            });
    }

    // ---- 2. the ranked stock names, with the band applied to the CHOICE ----
    let ranked = rank_names(name_moves(movers));
    let board = split_board(&ranked);
    let mut symbol_of: std::collections::HashMap<(u64, u8), &str> =
        std::collections::HashMap::new();
    for row in movers {
        symbol_of.insert(
            (row.security_id, row.segment.binary_code()),
            row.symbol.as_str(),
        );
    }

    let mut stock_instruments: Vec<SubscribeInstrument> =
        Vec::with_capacity(DEPTH20_NAME_STOCK_SOCKETS * DEPTH20_PER_SOCKET);
    for name in choose_names(&board, held) {
        if plan.chosen.len() >= DEPTH20_NAME_ENTRY_RANK {
            break;
        }
        let Some(symbol) = symbol_of.get(&name.key()).copied() else {
            plan.names_unresolved = plan.names_unresolved.saturating_add(1);
            continue;
        };
        let bucket = by_underlying.get(symbol).map_or(&[][..], Vec::as_slice);
        let spot = spot_for_bucket(candidates, symbol, bucket);
        let options = strike_window(
            bucket,
            spot,
            DEPTH20_STOCK_ATM_STRIKES_EACH_SIDE,
            crate::depth_rebalance::STOCK_OPTION_SEGMENT,
        );
        if options.is_empty() {
            // A name with no resolvable ladder takes no slot, and the next
            // preferred name gets the chance instead. Counted rather than
            // silently skipped: a rising count means the contract artifact and
            // the candle frames disagree about which stocks exist.
            plan.names_unresolved = plan.names_unresolved.saturating_add(1);
            continue;
        }
        // Assemble the name WHOLE into its own buffer first, then append.
        // A name that cannot be assembled must leave `seen` untouched, or it
        // blocks the contracts of the name that replaces it.
        let mut name_instruments: Vec<SubscribeInstrument> =
            Vec::with_capacity(slots_for_stock_name(DEPTH20_STOCK_ATM_STRIKES_EACH_SIDE));
        // The stock's own SPOT — the 2026-09-11 (FOURTH) grant. Admissible
        // where an index spot is not, because a cash equity is `NSE_EQ`, which
        // the vendor documents as supported.
        if crate::dhan_depth_universe::segment_supports_depth(name.segment) {
            claim_instrument(
                &mut name_instruments,
                SubscribeInstrument {
                    security_id: name.underlying_id,
                    segment: name.segment,
                },
                &mut seen,
            );
        } else {
            plan.spots_missing = plan.spots_missing.saturating_add(1);
        }
        match futures.get(&symbol.trim().to_ascii_uppercase()) {
            Some(future) => claim_instrument(&mut name_instruments, *future, &mut seen),
            None => plan.futures_missing = plan.futures_missing.saturating_add(1),
        }
        for instrument in options {
            claim_instrument(&mut name_instruments, instrument, &mut seen);
        }
        stock_instruments.extend(name_instruments);
        plan.chosen.push(name);
    }

    // ---- 3. pack the stock half across the remaining three sockets ----
    //
    // FLAT chunking, never one name per socket: the scope lock's 2026-09-11
    // (FOURTH) section refuses socket-affinity, because a name confined to one
    // socket draws on that socket's 4-swap budget alone while a name spread
    // across several draws on several.
    stock_instruments.truncate(DEPTH20_NAME_STOCK_SOCKETS * DEPTH20_PER_SOCKET);
    // `chunks(0)` panics, and this release profile aborts rather than unwinds,
    // so an empty stock half must not reach it. The upper bound is a socket
    // ceiling that the truncate above already guarantees; it is stated anyway
    // because "already guaranteed" is a property of the line above, not of
    // this one. `clamp` is total here: 1 <= DEPTH20_PER_SOCKET is a const fact.
    let chunk = stock_instruments
        .len()
        .div_ceil(DEPTH20_NAME_STOCK_SOCKETS)
        .clamp(1, DEPTH20_PER_SOCKET);
    let mut emitted = 0usize;
    for slice in stock_instruments.chunks(chunk) {
        if emitted >= DEPTH20_NAME_STOCK_SOCKETS {
            break;
        }
        plan.layout
            .sockets
            .push(crate::depth20_layout::Depth20Socket {
                underlying: None,
                instruments: slice.to_vec(),
            });
        emitted = emitted.saturating_add(1);
    }
    // Pad to the full pool. A missing socket would let `plan_depth20_minute`
    // pair a wire socket against a layout socket that is not its own.
    while emitted < DEPTH20_NAME_STOCK_SOCKETS {
        plan.layout
            .sockets
            .push(crate::depth20_layout::Depth20Socket {
                underlying: None,
                instruments: Vec::new(),
            });
        emitted = emitted.saturating_add(1);
    }
    plan
}

// ---------------------------------------------------------------------------
// OBSERVABILITY (2026-09-13): the PRIMARY depth-20 engine was measured by
// nothing at all.
// ---------------------------------------------------------------------------
//
// Until this section, `grep -c 'metrics::'` over this module returned **0**.
// Every outcome the board produces — how many names it chose, how many it
// could not resolve, how many lost a future or a spot, how many swaps the
// per-socket cap refused — existed ONLY inside the caller's `tracing::info!`,
// which is a line an operator reads after something ELSE has already told
// them to look.
//
// # Why absent is worse here than merely unmeasured
//
// The older `tv_depth20_ranked_*` series are incremented on the FALLBACK arm
// alone (`depth20_ranked_steer::record_depth20_ranked_decision`). From the
// minute this board takes over — ~09:07 daily, once the auction print gives
// the movers query a row to rank — those series FREEZE at whatever the last
// fallback minute wrote and never move again. A frozen counter and a counter
// correctly reporting a quiet pool are the same two numbers on a dashboard,
// so the handover does not merely stop producing signal: it converts the
// signal that IS there into a lie. That is the false-OK class
// `audit-findings-2026-04-17.md` Rule 11 forbids, and it is why the name
// board needs its OWN series rather than a share of the ranked ones.
//
// # Why every label value is a `&'static str` literal
//
// A non-literal label value drops `metrics::counter!` to its allocating arm.
// This repository has already paid for that once: `record_ws_lag` built a
// label with `to_string()` and allocated roughly 36 million times an hour on
// the path whose own docs called it allocation-free, and the fix
// (`dhat_ws_lag.rs`) is a build-failing gate precisely because three correct
// comments had not stopped it shipping. This is a once-a-minute steering
// path, so an allocation here would be harmless — the literals are used
// anyway, because the habit is what survives a function being moved.
//
// # Local `/metrics` ONLY — a decision, not an omission
//
// No EMF selector entry, no dashboard widget, no CloudWatch alarm. Each costs
// real money against a budget whose 90% line fires an AUTOMATIC
// `STOP_EC2_INSTANCES` on the trading box, and §2.3n of
// `dhan-rest-only-noise-lock-2026-07-14.md` makes the next addition of any
// size an OPERATOR decision that must arrive with a LEVER, never an
// executor's cost note. So these series reach the local exporter and stop
// there. They are the numbers an operator reads AFTER an existing page, and
// nothing here claims they page anyone.

/// Counter: one name-board planning minute, by outcome.
///
/// Every label is an EVENT count for the minute just planned, so a quiet
/// minute increments by zero and the series stays dense rather than sparse.
pub const DEPTH20_NAME_BOARD_COUNTER: &str = "tv_depth20_name_board_outcomes_total";

/// Gauge: STOCK names the last planning minute actually placed.
///
/// A LEVEL, not an event — the size of the board as it now stands, which is
/// the number a dashboard reads. Pre-registered at
/// [`NOT_YET_RANKED`] (`-1`)
/// rather than at zero, because zero is a REAL value here: it is the pre-09:07
/// state where no equity has printed and
/// [`NameBoardPlan::is_steerable`] is correctly false. Seeding at zero would
/// make "the board has never planned" and "the board planned and placed
/// nothing" the same reading, which is the distinction the whole series
/// exists to carry.
pub const DEPTH20_NAME_BOARD_CHOSEN_GAUGE: &str = "tv_depth20_name_board_names_chosen";

/// Every `outcome` label [`DEPTH20_NAME_BOARD_COUNTER`] carries.
///
/// Six, and each has a DIFFERENT remedy — which is the only reason they are
/// separate labels rather than one refusal count:
///
/// * `swaps_planned` — handed to the wire. The denominator for the rest.
/// * `swaps_capped` — refused by the per-socket per-minute budget. Remedy:
///   raise the cap or widen [`DEPTH20_NAME_EXIT_RANK`]. A standing non-zero
///   count is the read-out the band's own docstring names as the thing that
///   must move before that constant does.
/// * `names_unresolved` — a preferred name had no symbol or no resolvable
///   option ladder. Remedy: the contract artifact and the candle frames
///   disagree about which stocks exist; neither cap nor band would help.
/// * `index_unresolved` — NIFTY or BANKNIFTY had no window. This one alone
///   makes the plan unsteerable, so a non-zero count explains a minute the
///   caller fell back.
/// * `futures_missing` — a name placed without its future slot. Costs ONE
///   slot, never the name.
/// * `spots_missing` — a stock name placed without its `NSE_EQ` spot slot.
pub const DEPTH20_NAME_BOARD_OUTCOME_LABELS: [&str; 6] = [
    "swaps_planned",
    "swaps_capped",
    "names_unresolved",
    "index_unresolved",
    "futures_missing",
    "spots_missing",
];

/// Registers every name-board series before the first planning minute, so the
/// CloudWatch agent's dropped-first-sample rule cannot swallow the first real
/// event.
///
/// The counters seed at ZERO; the gauge seeds at [`NOT_YET_RANKED`], because
/// zero is one of its real values. Said here rather than left to the reader,
/// since "seeds everything at zero" would be the tidier sentence and the
/// wrong one.
///
/// The agent computes a counter as the DELTA between consecutive samples and
/// discards the first sample of a series it has never seen. A counter whose
/// first increment IS the event therefore publishes nothing on the one day it
/// matters — the shape that hid `tv_depth_rows_spilled_total` on 2026-08-28
/// and left 104,540 depth rows permanently unclassifiable, and which
/// `loss_series_seeding_guard.rs` now exists to forbid.
///
/// The gauge is deliberately NOT seeded at zero: see
/// [`DEPTH20_NAME_BOARD_CHOSEN_GAUGE`].
///
/// Call once at boot, before the first planning minute.
pub fn pre_register_name_board_counters() {
    metrics::gauge!(DEPTH20_NAME_BOARD_CHOSEN_GAUGE).set(NOT_YET_RANKED);
    for outcome in DEPTH20_NAME_BOARD_OUTCOME_LABELS {
        metrics::counter!(DEPTH20_NAME_BOARD_COUNTER, "outcome" => outcome).increment(0);
    }
}

/// Records one planning minute of the name board.
///
/// `swaps_planned` and `swaps_capped` are the CALLER's, not the plan's: this
/// module ranks names and lays out sockets, and deliberately does not decide
/// swaps (see the module header). They are taken as arguments rather than
/// read off `plan` so that the whole minute lands in ONE place — a plan
/// recorded here and a swap count recorded somewhere else would be two series
/// that drift apart the first time one call site is added without the other.
///
/// **Total**: every arm is a widening `usize` → `u64` cast (`usize` is never
/// wider than `u64` on any target this ships to), there is no indexing, no
/// division and no unwrap, so an empty or default
/// [`NameBoardPlan`] records six zeros and a zero gauge rather than panicking.
/// That matters because this runs on the steering task under
/// `panic = "abort"`: a recorder that can die takes the depth pool with it,
/// and observability must never be able to break the thing it observes.
pub fn record_name_board_plan(plan: &NameBoardPlan, swaps_planned: usize, swaps_capped: usize) {
    metrics::gauge!(DEPTH20_NAME_BOARD_CHOSEN_GAUGE).set(plan.chosen.len() as f64);
    // Six spelled-out calls rather than a loop over `(label, count)` pairs.
    // A loop reads tidier and is WRONG here: the label value would then be a
    // binding rather than a literal token at the macro site, which drops
    // `metrics::counter!` to the arm that builds a `Vec<Label>` per call. The
    // seeding loop above may do it because it runs once at boot; a per-minute
    // recorder keeps the literals. (`record_ws_lag`'s ~36M allocations an hour
    // came from exactly this distinction on a hotter path.)
    metrics::counter!(DEPTH20_NAME_BOARD_COUNTER, "outcome" => "swaps_planned")
        .increment(swaps_planned as u64);
    metrics::counter!(DEPTH20_NAME_BOARD_COUNTER, "outcome" => "swaps_capped")
        .increment(swaps_capped as u64);
    metrics::counter!(DEPTH20_NAME_BOARD_COUNTER, "outcome" => "names_unresolved")
        .increment(plan.names_unresolved as u64);
    metrics::counter!(DEPTH20_NAME_BOARD_COUNTER, "outcome" => "index_unresolved")
        .increment(plan.index_unresolved.len() as u64);
    metrics::counter!(DEPTH20_NAME_BOARD_COUNTER, "outcome" => "futures_missing")
        .increment(plan.futures_missing as u64);
    metrics::counter!(DEPTH20_NAME_BOARD_COUNTER, "outcome" => "spots_missing")
        .increment(plan.spots_missing as u64);
}

#[cfg(test)]
mod tests {
    use super::*;

    const EQ: ExchangeSegment = ExchangeSegment::NseEquity;

    fn n(id: u64, bps: i64) -> NameMove {
        NameMove {
            underlying_id: id,
            segment: EQ,
            move_bps: bps,
        }
    }

    #[test]
    fn move_bps_is_integer_basis_points_in_both_directions() {
        // +10% on a 100-rupee stock: 11000 paise against 10000.
        assert_eq!(move_bps(Some(11_000), Some(10_000)), Some(1_000));
        // -10% ranks IDENTICALLY under the absolute rule.
        assert_eq!(move_bps(Some(9_000), Some(10_000)), Some(1_000));
        // Flat is zero, and zero is a legitimate rank — unlike absent.
        assert_eq!(move_bps(Some(10_000), Some(10_000)), Some(0));
    }

    #[test]
    fn a_missing_price_is_not_rankable_and_is_never_zero() {
        // The 09:00-09:07 case. ~750 equities have no print at all until the
        // auction, and ranking them as 0% would tie them with a genuinely flat
        // name and hand one a socket it did not earn.
        assert_eq!(move_bps(None, Some(10_000)), None);
        assert_eq!(move_bps(Some(10_000), None), None);
        assert_eq!(move_bps(None, None), None);
    }

    #[test]
    fn move_bps_refuses_a_zero_or_negative_previous_close() {
        // A zero previous close is a LIVE wire value, not a corner case:
        // Ticker-mode packets carry 0.0, and the quote parser has a test
        // asserting it emits NaN. Dividing by it is what this refusal makes
        // unrepresentable.
        assert_eq!(move_bps(Some(10_000), Some(0)), None);
        assert_eq!(move_bps(Some(10_000), Some(-1)), None);
        assert_eq!(move_bps(Some(0), Some(10_000)), None);
        assert_eq!(move_bps(Some(-1), Some(10_000)), None);
    }

    #[test]
    fn move_bps_refuses_an_implausible_move_rather_than_ranking_it_first() {
        // eligible_gain_pct's own doc: "a previous close of 0.01 against a
        // 240-rupee LTP yields +2,400,000% - finite, sortable, and nonsense".
        // Finite and sortable is what makes it dangerous: it would pin rank 1
        // for the session.
        assert_eq!(move_bps(Some(24_000), Some(1)), None);
        // Exactly at the ceiling is still admitted.
        assert_eq!(
            move_bps(
                Some(10_000 + 10_000 * MAX_PLAUSIBLE_MOVE_BPS / 10_000),
                Some(10_000)
            ),
            Some(MAX_PLAUSIBLE_MOVE_BPS)
        );
    }

    /// The ceiling cannot bound a FALL, and with absolute ranking a fall is
    /// what reaches rank 1.
    ///
    /// Found by the 2026-09-11 adversarial sweep of this module. A spot of one
    /// paise against a ₹1,000 previous close is -9,999 bp; `abs()` makes it
    /// 9,999; the ceiling is 100,000; so before the floor existed this corrupt
    /// tick PASSED and took the top of the board for the minute.
    #[test]
    fn a_near_zero_spot_is_refused_and_never_takes_rank_one() {
        // One paise against 1,000 rupees: -99.99%, the worst a spot can print
        // without being non-positive (which is already refused above).
        assert_eq!(
            move_bps(Some(1), Some(100_000)),
            None,
            "a -99.99% tick passed the ceiling and ranked first"
        );
    }

    /// A 1:10 split prints the spot at a tenth of the UN-ADJUSTED previous
    /// close. That is a real, recurring, non-corrupt -90% that is not a move.
    #[test]
    fn an_ex_split_price_is_not_ranked_as_a_ninety_percent_faller() {
        assert_eq!(move_bps(Some(10_000), Some(100_000)), None);
        // A 1:2 split / 1:1 bonus lands NEAR -5,000 bp, and anything past the
        // floor is refused.
        assert_eq!(move_bps(Some(49_000), Some(100_000)), None);
        // Exactly ON the floor is ADMITTED, deliberately - the same rule as
        // the ceiling one test above, which admits exactly 100,000. Both
        // bounds mean "outside is refused"; making one of them
        // inclusive-refuse would be an asymmetry a reader has to remember,
        // and an auction equilibrium landing on the exact paise is not a
        // case worth that. The ex-date protection is the -90% arm above.
        assert_eq!(
            move_bps(Some(50_000), Some(100_000)),
            Some(MIN_PLAUSIBLE_MOVE_BPS.abs())
        );
    }

    /// The floor must be REACHABLE, which is the exact property the ceiling
    /// lacked on this side.
    #[test]
    fn the_floor_is_tighter_than_the_worst_fall_a_positive_spot_can_print() {
        // A spot of 1 paise against a huge close is the most negative reading
        // obtainable, and it must be OUTSIDE the band - otherwise the floor is
        // dead code exactly as the ceiling was.
        let worst = -(BPS_SCALE as i64) + 1;
        assert!(
            worst < MIN_PLAUSIBLE_MOVE_BPS,
            "the worst expressible fall is inside the band: the floor can never fire"
        );
    }

    /// A genuine big faller still ranks - the floor must not eat the signal
    /// the board exists to find.
    #[test]
    fn a_real_eight_percent_faller_still_ranks_and_ties_with_an_eight_percent_riser() {
        let faller = move_bps(Some(92_000), Some(100_000));
        let riser = move_bps(Some(108_000), Some(100_000));
        assert_eq!(faller, Some(800), "a -8% session must stay rankable");
        assert_eq!(riser, Some(800));
        assert_eq!(
            faller, riser,
            "absolute ranking: a faller ranks with a riser of the same size"
        );
    }

    #[test]
    fn move_bps_cannot_overflow_the_way_an_i64_multiply_would() {
        // The release profile sets overflow-checks = true AND panic = "abort",
        // so an i64 `delta * 10_000` overflow here is process DEATH on the
        // frame drain - the one task emptying the socket. SpotPriceStore
        // bounds paise only at 2^53, so a corrupt f32 reaches this function.
        //
        // The i128 multiply makes it arithmetic instead of an abort, and the
        // plausibility gate turns it into a refusal.
        let huge = 1_i64 << 53;
        // A doubling at the TOP of the paise range. `delta * 10_000` here is
        // 9.0e19 - past `i64::MAX` (9.2e18), so an i64 path aborts the process
        // and the i128 path returns the arithmetically correct 100% = 10,000 bp.
        // Asserting the VALUE rather than a refusal is the stronger test: it
        // proves the multiply happened, not merely that nothing crashed.
        assert_eq!(move_bps(Some(huge), Some(huge / 2)), Some(10_000));
        // The same magnitude against a 1-paise close is a 9e17% move, which is
        // a corrupt previous close rather than a stock, and is refused.
        assert_eq!(move_bps(Some(huge), Some(1)), None);
        // A large-but-ordinary pair still ranks, so the ceiling is not a
        // blanket refusal of big numbers.
        assert_eq!(move_bps(Some(2_000_000), Some(1_000_000)), Some(10_000));
    }

    #[test]
    fn rank_names_orders_by_move_descending_with_a_total_tie_break() {
        let ranked = rank_names(vec![n(30, 500), n(10, 900), n(20, 900), n(40, 100)]);
        // 900s first, and between them the LOWER id wins - deterministic.
        assert_eq!(
            ranked.iter().map(|r| r.underlying_id).collect::<Vec<_>>(),
            vec![10, 20, 30, 40]
        );
    }

    #[test]
    fn a_segment_twin_is_a_different_name_and_both_survive_the_ranking() {
        // I-P1-11: Dhan reuses one numeric id across segments. A board keyed on
        // the bare id would silently drop one of these.
        let ranked = rank_names(vec![
            NameMove {
                underlying_id: 13,
                segment: ExchangeSegment::IdxI,
                move_bps: 500,
            },
            NameMove {
                underlying_id: 13,
                segment: EQ,
                move_bps: 500,
            },
        ]);
        assert_eq!(ranked.len(), 2);
        assert_ne!(ranked[0].key(), ranked[1].key());
    }

    #[test]
    fn the_band_contains_the_entry_set_as_its_prefix() {
        let ranked = rank_names((1..=30).map(|i| n(i, 1_000 - i as i64)).collect());
        let board = split_board(&ranked);
        assert_eq!(board.entry.len(), DEPTH20_NAME_ENTRY_RANK);
        assert_eq!(board.band.len(), DEPTH20_NAME_EXIT_RANK);
        assert_eq!(&board.band[..DEPTH20_NAME_ENTRY_RANK], &board.entry[..]);
    }

    #[test]
    fn a_name_that_slips_out_of_the_entry_set_keeps_its_slots_inside_the_band() {
        // THE hysteresis test. Moving one name costs 24 of the 20 swaps a
        // minute affords, so a board that evicted on a single-rank slip could
        // never catch up to its own ranking.
        let ranked = rank_names((1..=30).map(|i| n(i, 1_000 - i as i64)).collect());
        let board = split_board(&ranked);
        let ninth = ranked[8];
        assert!(
            !board.admits(ninth.key()),
            "rank 9 must not CLAIM a slot it does not hold"
        );
        assert!(
            board.keeps(ninth.key()),
            "rank 9 must KEEP slots it already holds - that is the band"
        );
        let thirteenth = ranked[12];
        assert!(
            !board.keeps(thirteenth.key()),
            "past the band a held name finally gives its slots up"
        );
    }

    #[test]
    fn an_empty_ranking_admits_and_keeps_nothing() {
        let board = split_board(&[]);
        assert!(!board.admits((1, EQ.binary_code())));
        assert!(!board.keeps((1, EQ.binary_code())));
    }

    #[test]
    fn a_short_ranking_is_not_padded() {
        // Pre-open, before the auction print, the board legitimately has three
        // names. It must carry three, not three plus seven empties.
        let ranked = rank_names(vec![n(1, 300), n(2, 200), n(3, 100)]);
        let board = split_board(&ranked);
        assert_eq!(board.entry.len(), 3);
        assert_eq!(board.band.len(), 3);
    }

    #[test]
    fn the_authorized_shape_is_238_of_250() {
        // The operator asked directly: "will it sit under 250 slots". This is
        // the answer, and the const-assert above makes it a build failure
        // rather than a runtime pool refusal.
        assert_eq!(
            slots_for_index_name(DEPTH20_INDEX_ATM_STRIKES_EACH_SIDE),
            47
        );
        assert_eq!(
            slots_for_stock_name(DEPTH20_STOCK_ATM_STRIKES_EACH_SIDE),
            24
        );
        assert_eq!(board_slot_cost(), 238);
        assert!(board_slot_cost() <= DEPTH20_INSTRUMENT_BUDGET);
    }

    #[test]
    fn widening_the_stock_window_by_one_would_breach_the_budget() {
        // Why ±5 is a CEILING and not a preference. At ±6 the board is 262
        // against 250, and plan_pool refuses the WHOLE pool fail-closed - a
        // session with no depth at all, not a narrower one.
        let at_six = 2 * slots_for_index_name(DEPTH20_INDEX_ATM_STRIKES_EACH_SIDE)
            + DEPTH20_NAME_ENTRY_RANK * slots_for_stock_name(6);
        assert_eq!(at_six, 262);
        assert!(at_six > DEPTH20_INSTRUMENT_BUDGET);
    }

    #[test]
    fn moving_one_name_costs_more_swaps_than_a_minute_affords() {
        // The honest cost, pinned so it cannot be forgotten. 24 contracts leave
        // and 24 arrive when a name rotates; the pool can move 20 slots a
        // minute (4 per socket x 5), and ONE socket only 4 - so a rotation
        // confined to one line spans six minutes. That figure is the whole
        // argument for raising the swap budget, recorded in the 2026-09-11
        // (FOURTH) scope-lock section, and it is why the band above exists.
        const SWAPS_PER_MINUTE: usize = 20;
        assert!(slots_for_stock_name(DEPTH20_STOCK_ATM_STRIKES_EACH_SIDE) > SWAPS_PER_MINUTE);
    }

    #[test]
    fn split_board_takes_exactly_the_entry_and_exit_ranks_from_a_longer_list() {
        // The board is a CUT, never a copy. Fed twenty names it must hand back
        // seven and twelve - a board that grew with its input would hand the
        // placer more names than there are sockets to hold them, and the
        // refusal would land in plan_pool as a whole-pool fail-closed rather
        // than here as an arithmetic error.
        let ranked = rank_names(
            (0..20)
                .map(|i| NameMove {
                    underlying_id: 100 + i,
                    segment: ExchangeSegment::NseEquity,
                    // Descending, so rank order is the id order.
                    move_bps: (20 - i as i64) * 100,
                })
                .collect(),
        );
        let board = split_board(&ranked);
        assert_eq!(board.entry.len(), DEPTH20_NAME_ENTRY_RANK);
        assert_eq!(board.band.len(), DEPTH20_NAME_EXIT_RANK);
        // And it is the TOP of the list that survives, not an arbitrary slice.
        assert_eq!(board.entry[0].underlying_id, 100);
        assert_eq!(board.band[DEPTH20_NAME_EXIT_RANK - 1].underlying_id, 111);
    }

    #[test]
    fn admits_is_narrower_than_keeps_so_a_band_name_is_never_placed() {
        // The asymmetry IS the hysteresis. A name inside the band keeps the
        // slots it already holds, but nothing in the band may be PLACED: if
        // admits matched keeps, a name at rank 12 could take a socket from a
        // name at rank 8, which is churn wearing the costume of stability.
        let ranked = rank_names(
            (0..DEPTH20_NAME_EXIT_RANK as u64)
                .map(|i| NameMove {
                    underlying_id: 200 + i,
                    segment: ExchangeSegment::NseEquity,
                    move_bps: (DEPTH20_NAME_EXIT_RANK as i64 - i as i64) * 100,
                })
                .collect(),
        );
        let board = split_board(&ranked);
        let in_entry = (200_u64, ExchangeSegment::NseEquity.binary_code());
        let in_band_only = (
            200 + DEPTH20_NAME_ENTRY_RANK as u64,
            ExchangeSegment::NseEquity.binary_code(),
        );
        assert!(board.admits(in_entry) && board.keeps(in_entry));
        assert!(
            !board.admits(in_band_only) && board.keeps(in_band_only),
            "a band name must be kept but never placed"
        );
    }

    #[test]
    fn slots_for_name_counts_one_future_and_both_option_legs() {
        // A name is not one instrument. Getting this wrong understates the
        // budget by a factor of twenty-four and the error surfaces only as a
        // refused pool on a live morning.
        assert_eq!(slots_for_index_name(0), 3, "future + one CE + one PE");
        assert_eq!(slots_for_index_name(5), 1 + 11 * 2);
        assert_eq!(slots_for_index_name(11), 1 + 23 * 2);
    }

    #[test]
    fn a_stock_name_carries_its_spot_and_an_index_name_does_not() {
        // The one structural difference between the two formulae, pinned. An
        // index spot is IDX_I and validate_depth_segment refuses it outright,
        // so the index term can never grow this slot - a future reader adding
        // "symmetry" here would be asking for an instrument the wire rejects.
        for n in 0..=12 {
            assert_eq!(
                slots_for_stock_name(n),
                slots_for_index_name(n) + 1,
                "a stock name is its index shape plus exactly one NSE_EQ spot"
            );
        }
    }

    #[test]
    fn an_index_window_of_twelve_would_not_fit_one_socket() {
        // Why +/-11 is the operator's ceiling rather than his preference. This
        // is a SOCKET bound, independent of the 250 budget: at +/-12 one index
        // name needs 51 of a connection's 50, so its ladder would split across
        // two lines and neither would carry the whole book.
        assert_eq!(slots_for_index_name(11), 47);
        assert_eq!(slots_for_index_name(12), 51);
        assert!(slots_for_index_name(12) > DEPTH20_PER_SOCKET);
    }

    #[test]
    fn a_seventh_name_would_breach_the_budget() {
        // Why SIX is forced. The board carried seven names at index +/-10; at
        // +/-11 with the spot term a seventh is 262 against 250, and plan_pool
        // refuses the WHOLE pool fail-closed. Six is arithmetic, not taste.
        let at_seven = 2 * slots_for_index_name(DEPTH20_INDEX_ATM_STRIKES_EACH_SIDE)
            + 7 * slots_for_stock_name(DEPTH20_STOCK_ATM_STRIKES_EACH_SIDE);
        assert_eq!(at_seven, 262);
        assert!(at_seven > DEPTH20_INSTRUMENT_BUDGET);
    }

    #[test]
    fn board_slot_cost_is_the_sum_of_its_parts_and_fits_the_budget() {
        // Re-derived here rather than quoted, so a change to either window
        // moves this test with it instead of leaving a stale literal that
        // agrees with nothing.
        let expected = 2 * slots_for_index_name(DEPTH20_INDEX_ATM_STRIKES_EACH_SIDE)
            + DEPTH20_NAME_ENTRY_RANK * slots_for_stock_name(DEPTH20_STOCK_ATM_STRIKES_EACH_SIDE);
        assert_eq!(board_slot_cost(), expected);
        assert!(board_slot_cost() <= DEPTH20_INSTRUMENT_BUDGET);
    }

    // ---------------------------------------------------------------------
    // The WIRING (2026-09-13): the glue that turns a ranked name into a
    // layout. Every test below pins behaviour that had NO production caller
    // until this change — the skeleton-PR shape Rule 14 forbids.
    // ---------------------------------------------------------------------

    use crate::dhan_contract_universe::ContractRow;
    use crate::dhan_depth_universe::DepthCandidate;

    const FNO: ExchangeSegment = ExchangeSegment::NseFno;

    fn mover(id: u64, symbol: &str, pct: f64) -> MoverRow {
        MoverRow {
            security_id: id,
            segment: EQ,
            symbol: symbol.to_owned(),
            pct_change: pct,
        }
    }

    fn future_row(underlying: &str, expiry: u32, id: u64, class: &str, exch: &str) -> ContractRow {
        ContractRow {
            i: id,
            x: exch.to_owned(),
            c: class.to_owned(),
            e: expiry,
            s: 0,
            l: String::new(),
            u: underlying.to_owned(),
            z: 1,
        }
    }

    /// `strikes` steps of `step` rupees centred on `spot`, both legs, ids
    /// ascending from `id_base`.
    fn chain(u: &str, spot: f64, step: f64, strikes: i64, id_base: i64) -> Vec<DepthCandidate> {
        let mut out = Vec::new();
        let half = strikes / 2;
        for k in -half..=half {
            #[expect(clippy::cast_precision_loss, reason = "k is tiny, a strike index")]
            let strike = (spot / step).round() * step + (k as f64) * step;
            for (offset, leg) in [(0, "CE"), (1, "PE")] {
                out.push(DepthCandidate {
                    underlying: u.to_owned(),
                    contract_security_id: id_base + k * 2 + offset,
                    expiry_micros: 1_900_000_000_000_000,
                    strike,
                    spot,
                    leg: leg.to_owned(),
                    is_index_option: u == "NIFTY" || u == "BANKNIFTY",
                });
            }
        }
        out
    }

    fn refs(candidates: &[DepthCandidate]) -> Vec<&DepthCandidate> {
        candidates.iter().collect()
    }

    // ---- move_bps_from_pct: the SUBSTITUTION boundary ----

    #[test]
    fn move_bps_from_pct_is_integer_basis_points_and_absolute() {
        // 1% is 100 bp, and a faller ranks with a riser of the same size.
        assert_eq!(move_bps_from_pct(1.0), Some(100));
        assert_eq!(move_bps_from_pct(-1.0), Some(100));
        assert_eq!(move_bps_from_pct(0.0), Some(0));
        // Matches the live-price path for the same move, so the two sources
        // cannot disagree about the KEY even though they disagree about lag.
        assert_eq!(
            move_bps_from_pct(10.0),
            move_bps(Some(11_000), Some(10_000))
        );
    }

    #[test]
    fn move_bps_from_pct_refuses_the_values_that_would_pin_rank_one() {
        // NaN is not hypothetical: the quote parser has a test asserting the
        // wire emits it, and `close_pct_from_prev_day` divides by a close.
        assert_eq!(move_bps_from_pct(f64::NAN), None);
        assert_eq!(move_bps_from_pct(f64::INFINITY), None);
        assert_eq!(move_bps_from_pct(f64::NEG_INFINITY), None);
        // Past the ceiling: a corrupt previous close, not a stock.
        assert_eq!(move_bps_from_pct(2_400_000.0), None);
        // Past the FLOOR: an ex-split print is a real, recurring -90% that is
        // not a move. With absolute ranking it would otherwise take rank 1.
        assert_eq!(move_bps_from_pct(-90.0), None);
        // A real big faller still ranks - the floor must not eat the signal.
        assert_eq!(move_bps_from_pct(-8.0), Some(800));
    }

    #[test]
    fn name_moves_drops_an_unrankable_row_rather_than_ranking_it_zero() {
        let rows = vec![
            mover(1, "AAA", 2.0),
            mover(2, "BBB", f64::NAN),
            mover(3, "CCC", -90.0),
            mover(0, "ZERO", 1.0),
        ];
        let moves = name_moves(&rows);
        assert_eq!(
            moves.iter().map(|m| m.underlying_id).collect::<Vec<_>>(),
            vec![1],
            "a NaN move, an ex-split move and a zero id must be dropped, never carried at 0"
        );
        assert_eq!(moves[0].move_bps, 200);
    }

    // ---- future_index ----

    #[test]
    fn future_index_keeps_the_nearest_non_expired_future_per_underlying() {
        let rows = vec![
            future_row("NIFTY", 20_260_925, 101, "FUTIDX", "NSE"),
            future_row("NIFTY", 20_261_030, 102, "FUTIDX", "NSE"),
            // Already expired — must never be subscribed.
            future_row("NIFTY", 20_260_828, 103, "FUTIDX", "NSE"),
            future_row("RELIANCE", 20_260_925, 201, "FUTSTK", "NSE"),
            // Not a future at all.
            future_row("RELIANCE", 20_260_925, 202, "OPTSTK", "NSE"),
        ];
        let index = future_index(&rows, 20_260_913);
        assert_eq!(index.len(), 2);
        assert_eq!(index["NIFTY"].security_id, 101);
        assert_eq!(index["NIFTY"].segment, FNO);
        assert_eq!(index["RELIANCE"].security_id, 201);
    }

    #[test]
    fn future_index_refuses_a_segment_the_vendor_serves_no_depth_for() {
        // SENSEX futures are BSE_FNO and Dhan serves depth on NSE only, so a
        // SENSEX future here would be a socket that dies on connect.
        let rows = vec![future_row("SENSEX", 20_260_925, 301, "FUTIDX", "BSE")];
        assert!(future_index(&rows, 20_260_913).is_empty());
    }

    #[test]
    fn future_index_breaks_a_same_expiry_tie_on_the_lowest_id() {
        // The artifact carries no ORDER BY, so last-write-wins would let two
        // identical inputs choose different contracts and swap the socket back
        // and forth all session.
        let rows = vec![
            future_row("AAA", 20_260_925, 900, "FUTSTK", "NSE"),
            future_row("AAA", 20_260_925, 800, "FUTSTK", "NSE"),
        ];
        assert_eq!(future_index(&rows, 20_260_913)["AAA"].security_id, 800);
    }

    // ---- choose_names: THE hysteresis, applied to the CHOICE ----

    #[test]
    fn choose_names_keeps_an_incumbent_inside_the_band_over_a_higher_ranked_newcomer() {
        // Ranks 1..=20 by id. Held: the six names now ranked 7..=12 - all
        // inside the band, none inside the entry set.
        let ranked = rank_names((1..=20).map(|i| n(i, 1_000 - i as i64)).collect());
        let board = split_board(&ranked);
        let held: std::collections::BTreeSet<(u64, u8)> =
            (7..=12).map(|i| (i, EQ.binary_code())).collect();
        let chosen = choose_names(&board, &held);
        assert_eq!(
            chosen[..DEPTH20_NAME_ENTRY_RANK]
                .iter()
                .map(|c| c.underlying_id)
                .collect::<Vec<_>>(),
            vec![7, 8, 9, 10, 11, 12],
            "incumbents inside the band must be preferred over rank-1..6 newcomers - \
             that IS the band, and a band computed and then ignored is worse than none"
        );
    }

    #[test]
    fn choose_names_drops_an_incumbent_that_fell_out_of_the_band() {
        let ranked = rank_names((1..=20).map(|i| n(i, 1_000 - i as i64)).collect());
        let board = split_board(&ranked);
        // Rank 13 is one past DEPTH20_NAME_EXIT_RANK.
        let held: std::collections::BTreeSet<(u64, u8)> =
            [(13_u64, EQ.binary_code())].into_iter().collect();
        let chosen = choose_names(&board, &held);
        assert!(
            !chosen.iter().any(|c| c.underlying_id == 13),
            "past the band a held name finally gives its slots up"
        );
        assert_eq!(chosen[0].underlying_id, 1);
    }

    #[test]
    fn choose_names_lists_the_entry_set_behind_the_incumbents_as_a_fallback() {
        // Six incumbents fill the entry ranks, but the list must NOT stop
        // there: a name whose ladder does not resolve costs a slot the next
        // preferred name should get.
        let ranked = rank_names((1..=20).map(|i| n(i, 1_000 - i as i64)).collect());
        let board = split_board(&ranked);
        let held: std::collections::BTreeSet<(u64, u8)> =
            (7..=12).map(|i| (i, EQ.binary_code())).collect();
        let chosen = choose_names(&board, &held);
        assert_eq!(chosen.len(), DEPTH20_NAME_ENTRY_RANK * 2);
        assert_eq!(
            chosen[DEPTH20_NAME_ENTRY_RANK].underlying_id, 1,
            "the entry set follows the incumbents as the fallback tail"
        );
        // A band-ONLY name is never in the list to be PLACED.
        assert!(chosen.iter().all(|c| c.underlying_id <= 12));
    }

    // ---- strike_window ----

    #[test]
    fn strike_window_centres_on_spot_and_takes_both_legs() {
        let candidates = chain("AAA", 100.0, 10.0, 20, 5_000);
        let window = strike_window(&refs(&candidates), 100.0, 2, FNO);
        // 5 strikes x 2 legs.
        assert_eq!(window.len(), 10);
        assert!(window.iter().all(|i| i.segment == FNO));
        // The centre strike's CE id is the base; the window spans +/-2 steps.
        let ids: Vec<u64> = window.iter().map(|i| i.security_id).collect();
        assert!(
            ids.contains(&5_000),
            "the at-the-money CE must be in window"
        );
    }

    #[test]
    fn strike_window_never_pads_a_short_chain() {
        // Three strikes only. A +/-11 window must hand back three, not 23 -
        // a strike that does not exist cannot be subscribed, and inventing one
        // subscribes an id that returns silence forever.
        let candidates = chain("AAA", 100.0, 10.0, 2, 6_000);
        let window = strike_window(&refs(&candidates), 100.0, 11, FNO);
        assert_eq!(window.len(), 6);
    }

    #[test]
    fn strike_window_refuses_a_half_listed_strike_and_a_self_paired_one() {
        let mut candidates = chain("AAA", 100.0, 10.0, 4, 7_000);
        // Strip one leg from the top strike: the pair is the unit.
        let top = candidates.iter().map(|c| c.strike).fold(f64::MIN, f64::max);
        candidates.retain(|c| !(c.strike == top && c.leg == "PE"));
        // Make the bottom strike's two legs share one id.
        let bottom = candidates.iter().map(|c| c.strike).fold(f64::MAX, f64::min);
        for c in &mut candidates {
            if c.strike == bottom {
                c.contract_security_id = 7_777;
            }
        }
        let window = strike_window(&refs(&candidates), 100.0, 11, FNO);
        // Five strikes listed, two refused, three survive.
        assert_eq!(window.len(), 6);
        assert!(window.iter().all(|i| i.security_id != 7_777));
    }

    #[test]
    fn strike_window_refuses_a_spot_it_cannot_centre_on() {
        let candidates = chain("AAA", 100.0, 10.0, 4, 8_000);
        assert!(strike_window(&refs(&candidates), f64::NAN, 2, FNO).is_empty());
        assert!(strike_window(&refs(&candidates), 0.0, 2, FNO).is_empty());
        assert!(strike_window(&[], 100.0, 2, FNO).is_empty());
    }

    #[test]
    fn strike_window_keeps_only_the_nearest_expiry() {
        let mut candidates = chain("AAA", 100.0, 10.0, 4, 9_000);
        // A far month at the SAME strikes. Without the expiry filter it would
        // share a bucket and a far-month contract would be subscribed at a
        // strike chosen from this month's spot.
        let mut far = chain("AAA", 100.0, 10.0, 4, 9_500);
        for c in &mut far {
            c.expiry_micros = 2_000_000_000_000_000;
        }
        candidates.extend(far);
        let window = strike_window(&refs(&candidates), 100.0, 11, FNO);
        assert!(
            window.iter().all(|i| i.security_id < 9_500),
            "a far-month contract must never reach the window"
        );
    }

    // ---- build_name_layout ----

    /// Two index chains, six stock chains, and the movers that rank them.
    fn board_fixture() -> (Vec<DepthCandidate>, Vec<MoverRow>, Vec<ContractRow>) {
        let mut candidates = chain("NIFTY", 24_000.0, 50.0, 40, 100_000);
        candidates.extend(chain("BANKNIFTY", 52_000.0, 100.0, 40, 200_000));
        let mut movers = Vec::new();
        let mut futures = vec![
            future_row("NIFTY", 20_260_925, 1_001, "FUTIDX", "NSE"),
            future_row("BANKNIFTY", 20_260_925, 1_002, "FUTIDX", "NSE"),
        ];
        for k in 0..8u64 {
            let symbol = format!("STK{k}");
            #[expect(clippy::cast_precision_loss, reason = "k is 0..8")]
            let pct = 9.0 - (k as f64);
            candidates.extend(chain(
                &symbol,
                100.0,
                5.0,
                40,
                300_000 + i64::try_from(k).unwrap_or(0) * 1_000,
            ));
            movers.push(mover(700 + k, &symbol, pct));
            futures.push(future_row(&symbol, 20_260_925, 2_000 + k, "FUTSTK", "NSE"));
        }
        (candidates, movers, futures)
    }

    #[test]
    fn build_name_layout_emits_the_whole_pool_and_fits_the_budget() {
        let (candidates, movers, future_rows) = board_fixture();
        let futures = future_index(&future_rows, 20_260_913);
        let plan = build_name_layout(
            &candidates,
            &movers,
            &futures,
            &std::collections::BTreeSet::new(),
        );
        assert!(plan.is_steerable());
        assert_eq!(
            plan.layout.sockets.len(),
            crate::depth20_layout::DEPTH_20_SOCKETS,
            "the pool has five sockets and a short layout would let the tracker \
             pair a wire socket against a layout socket that is not its own"
        );
        assert_eq!(plan.chosen.len(), DEPTH20_NAME_ENTRY_RANK);
        assert_eq!(plan.layout.instrument_count(), board_slot_cost());
        assert!(plan.layout.instrument_count() <= DEPTH20_INSTRUMENT_BUDGET);
        for socket in &plan.layout.sockets {
            assert!(
                socket.instruments.len() <= DEPTH20_PER_SOCKET,
                "a socket wider than the connection spills onto the next one"
            );
        }
    }

    #[test]
    fn build_name_layout_gives_each_index_name_its_own_socket_and_never_ranks_it() {
        let (candidates, mut movers, future_rows) = board_fixture();
        // A stock moving 90% cannot displace NIFTY or BANKNIFTY. (It is
        // refused by the plausibility band too, which is the point: neither
        // route reaches the index sockets.)
        movers.push(mover(999, "HUGE", 90.0));
        let futures = future_index(&future_rows, 20_260_913);
        let plan = build_name_layout(
            &candidates,
            &movers,
            &futures,
            &std::collections::BTreeSet::new(),
        );
        assert_eq!(
            plan.layout.sockets[0].underlying.as_deref(),
            Some("NIFTY"),
            "socket 0 is NIFTY by POSITION - the tracker reads that"
        );
        assert_eq!(
            plan.layout.sockets[1].underlying.as_deref(),
            Some("BANKNIFTY")
        );
        assert_eq!(
            plan.layout.sockets[0].instruments.len(),
            slots_for_index_name(DEPTH20_INDEX_ATM_STRIKES_EACH_SIDE)
        );
        assert_eq!(
            plan.layout.sockets[1].instruments.len(),
            slots_for_index_name(DEPTH20_INDEX_ATM_STRIKES_EACH_SIDE)
        );
        // The index FUTURE is on the socket, and it leads it.
        assert_eq!(plan.layout.sockets[0].instruments[0].security_id, 1_001);
        assert!(plan.chosen.iter().all(|c| c.underlying_id != 999));
    }

    #[test]
    fn build_name_layout_carries_each_stock_name_spot_future_and_ladder() {
        let (candidates, movers, future_rows) = board_fixture();
        let futures = future_index(&future_rows, 20_260_913);
        let plan = build_name_layout(
            &candidates,
            &movers,
            &futures,
            &std::collections::BTreeSet::new(),
        );
        let stock: Vec<_> = plan.layout.sockets[2..]
            .iter()
            .flat_map(|s| s.instruments.iter().copied())
            .collect();
        assert_eq!(
            stock.len(),
            DEPTH20_NAME_ENTRY_RANK * slots_for_stock_name(DEPTH20_STOCK_ATM_STRIKES_EACH_SIDE)
        );
        // The top mover's own NSE_EQ spot - the 2026-09-11 (FOURTH) grant.
        assert!(
            stock
                .iter()
                .any(|i| i.security_id == 700 && i.segment == EQ),
            "a stock name must carry its own spot"
        );
        // and its nearest-expiry future.
        assert!(
            stock
                .iter()
                .any(|i| i.security_id == 2_000 && i.segment == FNO),
            "a stock name must carry its nearest-expiry future"
        );
        assert_eq!(plan.futures_missing, 0);
        assert_eq!(plan.spots_missing, 0);
    }

    #[test]
    fn build_name_layout_never_subscribes_one_instrument_twice() {
        let (candidates, movers, future_rows) = board_fixture();
        let futures = future_index(&future_rows, 20_260_913);
        let plan = build_name_layout(
            &candidates,
            &movers,
            &futures,
            &std::collections::BTreeSet::new(),
        );
        let mut seen = std::collections::HashSet::new();
        for socket in &plan.layout.sockets {
            for instrument in &socket.instruments {
                assert!(
                    seen.insert((instrument.security_id, instrument.segment.binary_code())),
                    "a duplicate inside one batch is an 804, which is Fatal: the \
                     connection drops and does not come back this session"
                );
            }
        }
    }

    #[test]
    fn build_name_layout_applies_the_band_when_choosing_the_names() {
        let (candidates, movers, future_rows) = board_fixture();
        let futures = future_index(&future_rows, 20_260_913);
        // Held: the two WEAKEST movers, ranked 7th and 8th - inside the band
        // (12), outside the entry set (6).
        let held: std::collections::BTreeSet<(u64, u8)> =
            [(706_u64, EQ.binary_code()), (707_u64, EQ.binary_code())]
                .into_iter()
                .collect();
        let plan = build_name_layout(&candidates, &movers, &futures, &held);
        let chosen: Vec<u64> = plan.chosen.iter().map(|c| c.underlying_id).collect();
        assert!(
            chosen.contains(&706) && chosen.contains(&707),
            "the exit band must GOVERN the choice: an incumbent at rank 7 keeps \
             its slots. If this fails the band is decoration and the board \
             re-orders freely while the swap counters report control."
        );
        assert_eq!(chosen.len(), DEPTH20_NAME_ENTRY_RANK);
        // And the next minute's input is what this minute chose.
        assert_eq!(plan.chosen_keys().len(), DEPTH20_NAME_ENTRY_RANK);
        assert!(plan.chosen_keys().contains(&(706, EQ.binary_code())));
    }

    /// An unresolvable name costs its own slot — it does NOT promote the next
    /// name in.
    ///
    /// Written asserting the opposite and CAUGHT here, which is why it is
    /// spelled out: only a name in the ENTRY set may ever be placed. Rank 7 is
    /// a band name, and the band's whole contract is *kept, never placed* —
    /// letting it fill a hole would be the one route by which a name outside
    /// the top six reaches a socket, and it would arrive through the error
    /// path rather than through the ranking.
    ///
    /// So the board carries five names and holds the sixth's slots empty for
    /// the minute. `plan_depth20_minute` leaves an empty desired socket
    /// completely alone, so the wire keeps what it has rather than being
    /// stripped on the strength of a missing chain.
    ///
    /// The incumbent case is DIFFERENT and is covered by
    /// `choose_names_lists_the_entry_set_behind_the_incumbents_as_a_fallback`:
    /// there the fallback is an entry-set name, which is allowed to be placed.
    #[test]
    fn an_unresolvable_entry_name_leaves_its_slots_empty_rather_than_promoting_a_band_name() {
        let (mut candidates, movers, future_rows) = board_fixture();
        // The top mover's chain vanishes - routine when the artifact and the
        // candle frames disagree about which stocks exist.
        candidates.retain(|c| c.underlying != "STK0");
        let futures = future_index(&future_rows, 20_260_913);
        let plan = build_name_layout(
            &candidates,
            &movers,
            &futures,
            &std::collections::BTreeSet::new(),
        );
        assert_eq!(plan.names_unresolved, 1);
        assert_eq!(
            plan.chosen.len(),
            DEPTH20_NAME_ENTRY_RANK - 1,
            "only an ENTRY-set name may be placed; rank 7 must not arrive through \
             the error path"
        );
        assert!(plan.chosen.iter().all(|c| c.underlying_id != 700));
        // Rank 7 (STK6, id 706) is in the band and stays out of the layout.
        assert!(plan.chosen.iter().all(|c| c.underlying_id != 706));
        assert!(plan.is_steerable());
    }

    #[test]
    fn an_empty_movers_ranking_is_not_steerable_and_the_caller_must_fall_back() {
        // The 09:00-09:07 case: ~750 equities have not printed at all, so the
        // movers query returns nothing. The board must decline rather than
        // displace the seed hold with an index-only layout.
        let (candidates, _, future_rows) = board_fixture();
        let futures = future_index(&future_rows, 20_260_913);
        let plan = build_name_layout(
            &candidates,
            &[],
            &futures,
            &std::collections::BTreeSet::new(),
        );
        assert!(plan.chosen.is_empty());
        assert!(
            !plan.is_steerable(),
            "with no stock name this is the legacy index layout at a different \
             width - it must not displace the engine already running"
        );
    }

    #[test]
    fn a_missing_index_chain_is_named_and_makes_the_plan_unsteerable() {
        let (mut candidates, movers, future_rows) = board_fixture();
        candidates.retain(|c| c.underlying != "BANKNIFTY");
        let futures = future_index(&future_rows, 20_260_913);
        let plan = build_name_layout(
            &candidates,
            &movers,
            &futures,
            &std::collections::BTreeSet::new(),
        );
        assert_eq!(plan.index_unresolved, vec!["BANKNIFTY".to_owned()]);
        assert!(
            !plan.is_steerable(),
            "NIFTY and BANKNIFTY are unconditional, so a plan missing one is not \
             the authorized board"
        );
        // The socket is still EMITTED - its position is its identity.
        assert_eq!(
            plan.layout.sockets.len(),
            crate::depth20_layout::DEPTH_20_SOCKETS
        );
        assert_eq!(
            plan.layout.sockets[1].underlying.as_deref(),
            Some("BANKNIFTY")
        );
    }

    #[test]
    fn a_missing_future_costs_one_slot_and_never_the_whole_name() {
        let (candidates, movers, _) = board_fixture();
        let plan = build_name_layout(
            &candidates,
            &movers,
            &std::collections::HashMap::new(),
            &std::collections::BTreeSet::new(),
        );
        assert!(
            plan.is_steerable(),
            "a name without a future is still a name"
        );
        // Two index names plus six stock names, each one future short.
        assert_eq!(plan.futures_missing, 2 + DEPTH20_NAME_ENTRY_RANK);
        assert_eq!(
            plan.layout.instrument_count(),
            board_slot_cost() - (2 + DEPTH20_NAME_ENTRY_RANK)
        );
    }

    // ---- NameBoardPlan::is_steerable / chosen_keys ----

    #[test]
    fn is_steerable_refuses_a_board_that_placed_no_stock_name() {
        // The SECOND condition, which the index-missing test above cannot
        // reach: both index windows resolve perfectly, and not one stock name
        // took a slot. That is the measured 09:00-09:07 shape - indices tick
        // from the pre-open, ~750 equities deliver nothing until the auction
        // print - so it is the NORMAL early state, not a fault.
        //
        // Steering on it would be actively harmful: the plan is the legacy
        // index layout at a different width, so it would displace the
        // validated seed hold and the volume ranking and give back nothing.
        let (mut candidates, movers, future_rows) = board_fixture();
        candidates.retain(|c| !c.underlying.starts_with("STK"));
        let futures = future_index(&future_rows, 20_260_913);
        let plan = build_name_layout(
            &candidates,
            &movers,
            &futures,
            &std::collections::BTreeSet::new(),
        );

        assert!(
            plan.index_unresolved.is_empty(),
            "both index windows resolved, so the FIRST condition passes: {:?}",
            plan.index_unresolved
        );
        assert!(
            plan.chosen.is_empty(),
            "no stock chain exists this minute, so nothing may be chosen"
        );
        assert!(
            !plan.is_steerable(),
            "an index-only board must hand back to the engine already running"
        );

        // And the converse, on the same fixture with the stocks restored -
        // otherwise this test would pass against a function hardcoded to false.
        let (candidates, movers, future_rows) = board_fixture();
        let futures = future_index(&future_rows, 20_260_913);
        let full = build_name_layout(
            &candidates,
            &movers,
            &futures,
            &std::collections::BTreeSet::new(),
        );
        assert!(full.is_steerable(), "the full board steers");
    }

    #[test]
    fn chosen_keys_are_the_composite_identity_and_feed_the_next_minute() {
        // Two names sharing ONE numeric id across segments. Dhan reuses ids
        // that way (I-P1-11), so a bare-id key would collapse these to one
        // entry and the next minute's hysteresis would hold the wrong name.
        let plan = NameBoardPlan {
            chosen: vec![
                NameMove {
                    underlying_id: 27,
                    segment: ExchangeSegment::NseEquity,
                    move_bps: 900,
                },
                NameMove {
                    underlying_id: 27,
                    segment: ExchangeSegment::IdxI,
                    move_bps: 800,
                },
            ],
            ..NameBoardPlan::default()
        };

        let keys = plan.chosen_keys();
        assert_eq!(keys.len(), 2, "a segment twin is TWO names, never one");
        assert!(keys.contains(&(27, ExchangeSegment::NseEquity.binary_code())));
        assert!(keys.contains(&(27, ExchangeSegment::IdxI.binary_code())));

        // The round-trip that makes the exit band mean anything: what the
        // board chose is exactly what `choose_names` reads back as `held`.
        // An incumbent inside the band outranks a higher-ranked newcomer, so
        // if the key shape did not match, the band would silently never bind.
        let ranked = rank_names(vec![
            n(99, 5_000), // a newcomer above both incumbents
            NameMove {
                underlying_id: 27,
                segment: ExchangeSegment::NseEquity,
                move_bps: 900,
            },
        ]);
        let board = split_board(&ranked);
        let held_next = plan.chosen_keys();
        let chosen = choose_names(&board, &held_next);
        assert!(
            chosen
                .iter()
                .any(|m| m.key() == (27, ExchangeSegment::NseEquity.binary_code())),
            "the incumbent must be recognised by the key the plan handed back"
        );
    }

    // ---- FIX 4: choose_names self-dedup ----

    /// One name is ONE entry, however many movers rows produced it.
    ///
    /// A duplicated `(security_id, segment)` in `movers` survives
    /// `name_moves` and `rank_names` as two independent entries, so the band
    /// can carry the same name twice. Before the step-1 self-dedup, step 1
    /// pushed BOTH copies; the second then reached `build_name_layout`,
    /// resolved the same symbol, found every one of its contracts already in
    /// `seen`, assembled an EMPTY instrument list — and still ran
    /// `plan.chosen.push(name)`, spending one of six slots on a name that
    /// subscribes nothing. Unreachable today because `instrument_lifecycle`'s
    /// DEDUP keys make the movers join 1:1; pinned so a query change cannot
    /// make it reachable silently.
    #[test]
    fn choose_names_never_lists_one_name_twice_from_a_duplicated_row() {
        let ranked = rank_names(vec![n(7, 900), n(7, 900), n(8, 800)]);
        let board = split_board(&ranked);
        assert_eq!(board.band.len(), 3, "the duplicate reaches the band");

        // HELD: the duplicate arrives through step 1, which had no dedup.
        let held: std::collections::BTreeSet<(u64, u8)> =
            [(7, EQ.binary_code())].into_iter().collect();
        let chosen = choose_names(&board, &held);
        assert_eq!(
            chosen.iter().filter(|c| c.underlying_id == 7).count(),
            1,
            "the incumbent pass must not list one name twice"
        );
        assert_eq!(chosen.len(), 2, "and the phantom must not pad the list");
        assert!(
            chosen.iter().any(|c| c.underlying_id == 8),
            "the genuine second name must still be reachable behind it"
        );

        // NOT held: the duplicate arrives through step 2, which always
        // deduped. Asserted so the fix cannot be removed on the grounds that
        // "the other pass covers it" — it covers only this half.
        let chosen = choose_names(&board, &std::collections::BTreeSet::new());
        assert_eq!(chosen.iter().filter(|c| c.underlying_id == 7).count(), 1);
        assert_eq!(chosen.len(), 2);
    }

    // ---- FIX 1: the name board's own metrics ----

    /// Every `outcome` label is distinct, and the names carry the house shape.
    ///
    /// A repeated label would silently merge two outcomes with DIFFERENT
    /// remedies — `swaps_capped` says raise the cap, `names_unresolved` says
    /// the contract artifact and the candle frames disagree and no cap change
    /// would help — into one number an operator cannot act on.
    #[test]
    fn the_name_board_outcome_labels_are_distinct_and_the_names_are_house_shaped() {
        let mut labels: Vec<&str> = DEPTH20_NAME_BOARD_OUTCOME_LABELS.to_vec();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(
            labels.len(),
            DEPTH20_NAME_BOARD_OUTCOME_LABELS.len(),
            "a repeated outcome label merges two different remedies"
        );

        assert!(DEPTH20_NAME_BOARD_COUNTER.starts_with("tv_depth20_name_board_"));
        assert!(
            DEPTH20_NAME_BOARD_COUNTER.ends_with("_total"),
            "a counter is _total; the suffix is what says it is monotonic"
        );
        assert!(DEPTH20_NAME_BOARD_CHOSEN_GAUGE.starts_with("tv_depth20_name_board_"));
        assert!(
            !DEPTH20_NAME_BOARD_CHOSEN_GAUGE.ends_with("_total"),
            "the gauge is a LEVEL and must not wear the counter suffix"
        );
    }

    /// The seeding call and the recorder are TOTAL.
    ///
    /// Both run on the steering task under `panic = "abort"`, so a recorder
    /// that could index, divide or unwrap would take the whole depth pool down
    /// with it — observability must never be able to break the thing it
    /// observes. The empty-plan arm is not a contrived input: it is the
    /// pre-09:07 shape, where ~750 equities have not printed and the board
    /// legitimately places nothing.
    #[test]
    fn the_name_board_counters_seed_and_the_recorder_survives_an_empty_plan() {
        pre_register_name_board_counters();
        record_name_board_plan(&NameBoardPlan::default(), 0, 0);

        // A default plan must genuinely be the zero case, or the line above
        // proves nothing about the arms that read real counts.
        let empty = NameBoardPlan::default();
        assert!(empty.chosen.is_empty());
        assert!(empty.index_unresolved.is_empty());
        assert_eq!(
            (
                empty.names_unresolved,
                empty.futures_missing,
                empty.spots_missing
            ),
            (0, 0, 0)
        );
        assert!(!empty.is_steerable(), "and it is the unsteerable shape");
    }

    /// The recorder on a REAL plan, with an anti-vacuity gate.
    ///
    /// Written this way because the obvious version — build a plan, record it,
    /// assert nothing panicked — passes just as happily against a fixture that
    /// resolved nothing at all, which is the case the empty-plan test already
    /// covers. The assertions below make this test measure the arms that carry
    /// non-zero counts.
    #[test]
    fn record_name_board_plan_covers_a_plan_that_actually_placed_names() {
        pre_register_name_board_counters();
        let (candidates, movers, future_rows) = board_fixture();
        let futures = future_index(&future_rows, 20_260_913);
        let plan = build_name_layout(
            &candidates,
            &movers,
            &futures,
            &std::collections::BTreeSet::new(),
        );
        assert!(
            plan.is_steerable(),
            "the fixture must produce a steerable board, or this measures nothing"
        );
        assert_eq!(
            plan.chosen.len(),
            DEPTH20_NAME_ENTRY_RANK,
            "a full board is the six the gauge is supposed to report"
        );

        // `swaps_planned`/`swaps_capped` are the caller's numbers, so they are
        // exercised with a shape the wire can really produce: moving one name
        // costs 24 swaps against the 20 a minute affords.
        record_name_board_plan(&plan, 20, 4);
    }
}
