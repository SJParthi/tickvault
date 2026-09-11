//! The depth-20 NAME board: top 7 stock underlyings by absolute percentage
//! move, plus NIFTY and BANKNIFTY unconditionally (operator, 2026-09-11).
//!
//! Authorized by the "2026-09-11 (THIRD)" section of
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

/// How many STOCK underlyings enter the board.
///
/// The operator's number, verbatim: *"pcik the top 7"*.
pub const DEPTH20_NAME_ENTRY_RANK: usize = 7;

/// How far a held name may slip before it loses its slots.
///
/// A held name is KEPT while its rank is `<= DEPTH20_NAME_EXIT_RANK`, so it
/// must fall out of the top 12 (~6% of the ~208 live F&O underlyings) before
/// its 23 contracts are given away.
///
/// **This band is the remedy the 2026-09-07 lock prescribes in advance**, not
/// an invention: *"If the swap budget is hit routinely, the answer is a longer
/// window or a hysteresis band on entry/exit"*. It is needed because a
/// percentage-move key re-orders in BOTH directions every minute, unlike
/// cumulative volume which only ever rises — and moving ONE name costs 23 of
/// the 20 swaps a minute affords, so a board that re-ordered freely could never
/// catch up to its own ranking.
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
/// `2 × (1 + 21 × 2) + 7 × (1 + (2N+1) × 2)` against a hard 250. At `N = 5`
/// that is 247; at `N = 6` it is 275, and `plan_pool` refuses the WHOLE pool
/// fail-closed rather than truncating — a session-ending failure, not a
/// degraded one.
pub const DEPTH20_STOCK_ATM_STRIKES_EACH_SIDE: usize = 5;

/// Strikes each side of at-the-money for NIFTY and BANKNIFTY.
///
/// The operator's number, verbatim: *"for index only nifty and bankn ifty
/// futures and its repscetive options atm plus or min us 10"*.
pub const DEPTH20_INDEX_ATM_STRIKES_EACH_SIDE: usize = 10;

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

/// Basis points per unit ratio.
const BPS_SCALE: i128 = 10_000;

/// The absolute move of one underlying, in integer basis points.
///
/// `None` — and the caller must treat it as NOT RANKABLE, never as zero — when:
///
/// * the previous close is absent or non-positive (a zero close is a live
///   Ticker-mode sentinel, and dividing by it is the defect this signature
///   exists to make unrepresentable);
/// * the spot is absent or non-positive;
/// * the computed move exceeds [`MAX_PLAUSIBLE_MOVE_BPS`].
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
    let bps = if DEPTH20_RANK_ABSOLUTE_MOVE {
        signed_bps.abs()
    } else {
        signed_bps
    };
    // Bounded back into i64 by the plausibility gate, so the cast cannot
    // truncate: anything that would not fit is far past the ceiling.
    if bps > i128::from(MAX_PLAUSIBLE_MOVE_BPS) || bps < -i128::from(MAX_PLAUSIBLE_MOVE_BPS) {
        return None;
    }
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

/// Slots one name consumes: its nearest-expiry future, plus both legs of every
/// strike in its window.
///
/// `1 + (2N+1) × 2`. Every unique F&O contract is one subscription — a future
/// costs exactly what an option costs, because the pool dedups on the I-P1-11
/// composite and instrument class is invisible to the subscribe path.
#[must_use]
pub const fn slots_for_name(strikes_each_side: usize) -> usize {
    1 + (2 * strikes_each_side + 1) * 2
}

/// The whole board's slot cost: two index names plus the stock entry set.
#[must_use]
pub const fn board_slot_cost() -> usize {
    2 * slots_for_name(DEPTH20_INDEX_ATM_STRIKES_EACH_SIDE)
        + DEPTH20_NAME_ENTRY_RANK * slots_for_name(DEPTH20_STOCK_ATM_STRIKES_EACH_SIDE)
}

/// The authorized shape must fit the socket budget — at compile time, because
/// discovering it at runtime means `plan_pool` refusing the WHOLE pool
/// fail-closed and a session with no depth at all.
const _: () = assert!(
    board_slot_cost() <= DEPTH20_INSTRUMENT_BUDGET,
    "the depth-20 name board must fit 250 instruments; widen the stock ATM \
     window and it does not — 247 at ±5, 275 at ±6"
);

/// The band must be strictly wider than the entry set, or there is no
/// hysteresis at all and a name loses 23 slots on one minute's slip.
const _: () = assert!(
    DEPTH20_NAME_EXIT_RANK > DEPTH20_NAME_ENTRY_RANK,
    "the keep band must be wider than the entry set"
);

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
    fn a_name_that_slips_out_of_the_top_seven_keeps_its_slots_inside_the_band() {
        // THE hysteresis test. Moving one name costs 23 of the 20 swaps a
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
    fn the_authorized_shape_is_247_of_250() {
        // The operator asked directly: "will it sit under 250 slots". This is
        // the answer, and the const-assert above makes it a build failure
        // rather than a runtime pool refusal.
        assert_eq!(slots_for_name(DEPTH20_INDEX_ATM_STRIKES_EACH_SIDE), 43);
        assert_eq!(slots_for_name(DEPTH20_STOCK_ATM_STRIKES_EACH_SIDE), 23);
        assert_eq!(board_slot_cost(), 247);
        assert!(board_slot_cost() <= DEPTH20_INSTRUMENT_BUDGET);
    }

    #[test]
    fn widening_the_stock_window_by_one_would_breach_the_budget() {
        // Why ±5 is a CEILING and not a preference. At ±6 the board is 275
        // against 250, and plan_pool refuses the WHOLE pool fail-closed - a
        // session with no depth at all, not a narrower one.
        let at_six = 2 * slots_for_name(DEPTH20_INDEX_ATM_STRIKES_EACH_SIDE)
            + DEPTH20_NAME_ENTRY_RANK * slots_for_name(6);
        assert_eq!(at_six, 275);
        assert!(at_six > DEPTH20_INSTRUMENT_BUDGET);
    }

    #[test]
    fn moving_one_name_costs_more_swaps_than_a_minute_affords() {
        // The honest cost, pinned so it cannot be forgotten. 23 contracts leave
        // and 23 arrive when a name rotates; the pool can move 20 slots a
        // minute (4 per socket x 5). So one rotation spans two minutes - which
        // is exactly why the band above exists.
        const SWAPS_PER_MINUTE: usize = 20;
        assert!(slots_for_name(DEPTH20_STOCK_ATM_STRIKES_EACH_SIDE) > SWAPS_PER_MINUTE);
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
        // budget by a factor of twenty-three and the error surfaces only as a
        // refused pool on a live morning.
        assert_eq!(slots_for_name(0), 3, "future + one CE + one PE");
        assert_eq!(slots_for_name(5), 1 + 11 * 2);
        assert_eq!(slots_for_name(10), 1 + 21 * 2);
    }

    #[test]
    fn board_slot_cost_is_the_sum_of_its_parts_and_fits_the_budget() {
        // Re-derived here rather than quoted, so a change to either window
        // moves this test with it instead of leaving a stale literal that
        // agrees with nothing.
        let expected = 2 * slots_for_name(DEPTH20_INDEX_ATM_STRIKES_EACH_SIDE)
            + DEPTH20_NAME_ENTRY_RANK * slots_for_name(DEPTH20_STOCK_ATM_STRIKES_EACH_SIDE);
        assert_eq!(board_slot_cost(), expected);
        assert!(board_slot_cost() <= DEPTH20_INSTRUMENT_BUDGET);
    }
}
