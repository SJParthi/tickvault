//! Properties of the ranking that fills three of the five 20-level sockets.
//!
//! # Why this surface
//!
//! `rank_movers` picks the day's 75 biggest movers, and their contracts fill
//! sockets three, four and five. It is the function whose EMPTY result on the
//! morning of 26 August meant those three sockets were never dialed at all —
//! depth-20 ran on two connections of five, 100 instruments instead of 250,
//! with every dashboard green.
//!
//! It is also arithmetic with an overlap in it. Fewer than 74 movers and the
//! head and the tail of one ordering meet, so the gainers and the losers can
//! collide, and the seventy-fifth stock can be one already taken. Those
//! failures need a specific COUNT to appear — which is exactly what a
//! generated test varies and a hand-written one fixes at whatever number the
//! author happened to pick.
//!
//! # The honest limit
//!
//! A passing run means no counterexample was found among the cases tried. The
//! input space is infinite; these widen the search rather than closing it.

use proptest::prelude::*;
use std::collections::BTreeSet;

use tickvault_app::movers::{MOVER_STOCKS_TOTAL, MOVERS_PER_SIDE, StockMove, rank_movers};
use tickvault_common::types::ExchangeSegment;

const SEGMENTS: [ExchangeSegment; 3] = [
    ExchangeSegment::NseEquity,
    ExchangeSegment::BseEquity,
    ExchangeSegment::IdxI,
];

fn key(m: &StockMove) -> (u64, u8) {
    (m.security_id, m.segment as u8)
}

/// Moves over a SMALL id space and a small set of percentages.
///
/// Both are deliberate. Small ids make duplicate `(security_id, segment)`
/// pairs common — a stock ranked twice takes two of the 74 slots and
/// subscribes the same contracts twice, which Dhan answers with 804. And ids
/// repeated ACROSS segments are the I-P1-11 case: 13 is NIFTY in `IDX_I` and
/// an unrelated cash stock in `NSE_EQ`.
///
/// Repeated percentages make TIES common, and ties are where an ordering
/// stops being a total order and starts depending on input arrangement.
fn stock_move() -> impl Strategy<Value = StockMove> {
    (
        0_u64..30,
        0_usize..SEGMENTS.len(),
        prop_oneof![
            // The refusable and the excluded, at a realistic rate.
            1 => Just(f64::NAN),
            1 => Just(f64::INFINITY),
            2 => Just(0.0_f64),
            // A coarse grid, so ties happen constantly.
            8 => (-20_i32..21).prop_map(|p| f64::from(p) / 2.0),
        ],
    )
        .prop_map(|(security_id, s, pct_change)| StockMove {
            security_id,
            segment: SEGMENTS[s],
            pct_change,
        })
}

fn moves(max: usize) -> impl Strategy<Value = Vec<StockMove>> {
    prop::collection::vec(stock_move(), 0..max)
}

/// A WIDE id space, for the properties that only mean anything ABOVE the cap.
///
/// The narrow generator above is right for collisions and ties and wrong for
/// capacity, and the difference is not academic: with ids from 0..30 across
/// three segments there are 90 distinct stocks at most, and roughly a fifth of
/// generated moves are refused as flat or non-finite. So the usable count
/// essentially never passes 74 — and every property about the 37-a-side cap,
/// about both sides being full, and about the seventy-fifth slot was passing
/// on inputs that could not reach the branch it was written for.
///
/// Caught by breaking the code on purpose: disabling the seventy-fifth slot
/// entirely left all ten properties green. A property that cannot reach its
/// own branch is the same failure as a property that cannot fail, arriving by
/// a different route — the assertion is fine and the INPUTS never get there.
fn wide_move() -> impl Strategy<Value = StockMove> {
    (
        0_u64..400,
        0_usize..SEGMENTS.len(),
        prop_oneof![
            1 => Just(0.0_f64),
            9 => (-400_i32..401).prop_map(|p| f64::from(p) / 20.0),
        ],
    )
        .prop_map(|(security_id, s, pct_change)| StockMove {
            security_id,
            segment: SEGMENTS[s],
            pct_change,
        })
}

fn wide_moves() -> impl Strategy<Value = Vec<StockMove>> {
    prop::collection::vec(wide_move(), 90..260)
}

proptest! {
    /// NO STOCK IS SELECTED TWICE, ACROSS ALL THREE LISTS.
    ///
    /// The gainers and the losers are the head and the tail of ONE ordering.
    /// Below 74 movers those meet, and a stock in both lists takes two of the
    /// 74 slots and subscribes its contracts twice — 804, and the connection
    /// is gone for the session. The seventy-fifth stock is the same failure
    /// one step further along.
    #[test]
    fn no_stock_appears_in_more_than_one_of_the_three_lists(m in moves(90)) {
        let got = rank_movers(&m);
        let mut seen: BTreeSet<(u64, u8)> = BTreeSet::new();
        for stock in got
            .gainers
            .iter()
            .chain(got.losers.iter())
            .chain(got.tiebreak.iter())
        {
            prop_assert!(seen.insert(key(stock)), "{:?} selected twice", key(stock));
        }
    }

    /// THE BUDGET. Never more than 37 a side, never more than 75 in total.
    #[test]
    fn neither_side_exceeds_its_share_and_the_total_holds(m in wide_moves()) {
        let got = rank_movers(&m);
        prop_assert!(got.gainers.len() <= MOVERS_PER_SIDE);
        prop_assert!(got.losers.len() <= MOVERS_PER_SIDE);
        prop_assert!(got.selected() <= MOVER_STOCKS_TOTAL);
    }

    /// BOTH SIDES ARE REPRESENTED WHENEVER BOTH EXIST.
    ///
    /// Capping each side at 37 BEFORE splitting would be wrong in a way that
    /// looks right: on a two-mover day it hands both to the gainers and leaves
    /// the losers empty, so a socket dialed to watch the day's worst faller
    /// watches nothing while the day plainly has one.
    #[test]
    fn a_day_with_at_least_two_movers_fills_both_sides(m in moves(90)) {
        let got = rank_movers(&m);
        let usable = m
            .iter()
            .filter(|x| x.pct_change.is_finite() && x.pct_change != 0.0)
            .map(key)
            .collect::<BTreeSet<_>>()
            .len();
        if usable >= 2 {
            prop_assert!(!got.gainers.is_empty(), "no gainers from {usable} movers");
            prop_assert!(!got.losers.is_empty(), "no losers from {usable} movers");
        }
    }

    /// NOTHING IS INVENTED, and every selected stock actually moved.
    ///
    /// A flat stock in a movers socket is a socket watching nothing happen.
    /// A non-finite percentage is a stamping fault upstream and must be
    /// refused rather than ranked into an arbitrary position.
    #[test]
    fn every_selected_stock_came_from_the_input_and_actually_moved(m in moves(90)) {
        let offered: BTreeSet<(u64, u8)> = m.iter().map(key).collect();
        let got = rank_movers(&m);
        for stock in got
            .gainers
            .iter()
            .chain(got.losers.iter())
            .chain(got.tiebreak.iter())
        {
            prop_assert!(offered.contains(&key(stock)), "invented {stock:?}");
            prop_assert!(stock.pct_change.is_finite(), "ranked a non-number");
            prop_assert_ne!(stock.pct_change, 0.0, "ranked a flat stock");
        }
    }

    /// THE ORDER WITHIN EACH LIST IS THE ORDER THE SOCKETS ARE FILLED IN.
    ///
    /// Gainers strongest first, losers steepest first. The packing takes them
    /// in order, so a mis-ordered list does not merely read oddly — it puts
    /// the wrong stocks on the sockets when the list is longer than the space.
    #[test]
    fn each_list_is_ordered_by_how_hard_the_stock_moved(m in wide_moves()) {
        let got = rank_movers(&m);
        for pair in got.gainers.windows(2) {
            prop_assert!(pair[0].pct_change >= pair[1].pct_change, "gainers out of order");
        }
        for pair in got.losers.windows(2) {
            prop_assert!(pair[0].pct_change <= pair[1].pct_change, "losers out of order");
        }
    }

    /// A GAINER ROSE AND A LOSER FELL, whenever both sides exist.
    ///
    /// On a thin day the single ordering is split at its midpoint, so a
    /// "loser" can legitimately be a small riser — there is nothing else to
    /// put there. But once BOTH signs are present in quantity, a stock on the
    /// wrong side means a socket is watching the opposite of what it was
    /// dialed for.
    #[test]
    fn with_both_signs_present_the_sides_are_not_crossed(m in wide_moves()) {
        let got = rank_movers(&m);
        // Counted from the FIRST row per stock, which is the row the ranking
        // keeps. Counting a stock as a riser because ANY of its rows was
        // positive over-counts: a stock listed twice, once up and once down,
        // is whichever row arrived first, and asking the question of the rows
        // rather than of the stock made this property fail the code for
        // behaving correctly.
        let mut first: Vec<StockMove> = Vec::new();
        let mut seen_first: BTreeSet<(u64, u8)> = BTreeSet::new();
        for x in &m {
            if !x.pct_change.is_finite() || x.pct_change == 0.0 {
                continue;
            }
            if seen_first.insert(key(x)) {
                first.push(*x);
            }
        }
        let risers = first.iter().filter(|x| x.pct_change > 0.0).count();
        let fallers = first.iter().filter(|x| x.pct_change < 0.0).count();
        // Only assert where the split cannot be forced to borrow across zero.
        if risers >= MOVERS_PER_SIDE && fallers >= MOVERS_PER_SIDE {
            for g in &got.gainers {
                prop_assert!(g.pct_change > 0.0, "a faller in the gainers: {g:?}");
            }
            for l in &got.losers {
                prop_assert!(l.pct_change < 0.0, "a riser in the losers: {l:?}");
            }
        }
    }

    /// THE SEVENTY-FIFTH SLOT IS FILLED EXACTLY WHEN THERE IS SOMEONE TO
    /// FILL IT WITH.
    ///
    /// Left empty while a mover was available, two socket slots watch nothing
    /// on a day that had something to watch. Filled when nothing was left, it
    /// would have to be a stock already selected — the overlap failure one
    /// step past the gainers and losers.
    ///
    /// **This property replaced one of mine that could not fail.** The first
    /// version asserted that the seventy-fifth stock's move was above zero and
    /// that the best remaining move was at least zero — both true by
    /// construction of everything upstream, so it would have passed against
    /// any implementation at all. Written to check that the harder side wins,
    /// it checked nothing. The claim below is narrower and actually
    /// falsifiable, which is worth more than a broad claim that is not.
    #[test]
    fn the_seventy_fifth_slot_is_filled_when_and_only_when_a_mover_remains(
        m in wide_moves(),
    ) {
        let got = rank_movers(&m);
        let usable = m
            .iter()
            .filter(|x| x.pct_change.is_finite() && x.pct_change != 0.0)
            .map(key)
            .collect::<BTreeSet<_>>()
            .len();
        let taken = got.gainers.len() + got.losers.len();
        prop_assert_eq!(
            got.tiebreak.is_some(),
            usable > taken,
            "{} usable movers, {} taken, tiebreak {:?}",
            usable,
            taken,
            got.tiebreak.map(|t| key(&t))
        );
    }

    /// THE COUNTERS DESCRIBE WHAT HAPPENED.
    ///
    /// Before 09:15 EVERY stock is flat, so a session where `skipped_flat`
    /// never falls is a session where the percentages never got computed —
    /// which is exactly the shape that left three sockets dark. The counters
    /// are how that is told apart from a genuinely quiet market.
    #[test]
    fn the_refusal_counters_match_the_input(m in moves(90)) {
        let got = rank_movers(&m);
        let non_finite = m.iter().filter(|x| !x.pct_change.is_finite()).count();
        let flat = m
            .iter()
            .filter(|x| x.pct_change.is_finite() && x.pct_change == 0.0)
            .count();
        prop_assert_eq!(got.refused_non_finite, non_finite);
        prop_assert_eq!(got.skipped_flat, flat);
        // Everything that survived refusal is either selected or dropped as a
        // duplicate or left over — the counters must not double-count.
        prop_assert!(got.dropped_duplicates <= m.len());
    }

    /// DETERMINISTIC AND ORDER-INDEPENDENT.
    ///
    /// The percentages come from a query with no `ORDER BY`. A ranking that
    /// depends on arrival order would swap three sockets' worth of stocks on
    /// identical market data.
    #[test]
    fn reversing_the_input_does_not_change_the_selected_set(m in moves(90)) {
        let forward = rank_movers(&m);
        let reversed: Vec<StockMove> = m.iter().rev().copied().collect();
        let backward = rank_movers(&reversed);
        let set = |s: &tickvault_app::movers::MoverSelection| {
            s.gainers
                .iter()
                .chain(s.losers.iter())
                .chain(s.tiebreak.iter())
                .map(key)
                .collect::<BTreeSet<_>>()
        };
        prop_assert_eq!(set(&forward), set(&backward));
    }

    /// Never panics. Float ordering with ties, an overlap that only appears
    /// below a specific count, and three interacting cuts.
    #[test]
    fn it_never_panics(m in moves(200)) {
        let _ = rank_movers(&m);
    }
}
