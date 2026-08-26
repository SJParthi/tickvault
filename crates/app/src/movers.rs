//! Today's biggest movers — the ranked gainers and losers that fill depth-20
//! sockets 3 to 5.
//!
//! # Why this exists (2026-08-26)
//!
//! The operator's depth-20 rule gives sockets 1 and 2 to NIFTY and BANKNIFTY
//! and the remaining three to *"top gainers 25 and top losers 25 one and only
//! from fno stocks alone"*, later widened to **37 each** so the 150 slots fill
//! exactly: 37 + 37 = 74 stocks, at an at-the-money call and put each, is 148
//! contracts.
//!
//! No such ranking existed anywhere in this system — not in the database, not
//! in memory, not on any page. A search on 2026-08-26 found only log-target
//! strings. So the three sockets had nothing to fill them with.
//!
//! # Which percentage ranks them, and why it is not a detail
//!
//! **`close_pct_from_prev_day`** — the move against YESTERDAY'S CLOSE. That is
//! what "percentage change" means on every market screen in India, and it is
//! what the operator confirmed when he corrected my labelling on 2026-08-26.
//!
//! Ranked on the other column — the move since today's 09:15 open — the list
//! is *plausible and completely different*, and nothing about it would look
//! wrong. Varun Beverages on 2026-08-26 is the standing example: it gapped up
//! 2.26% overnight and then fell 5.93% from that open, so it sits near the top
//! of one ranking and near the bottom of the other on the same day.
//!
//! # This module is pure
//!
//! It takes a slice of already-measured moves and returns a ranking. It reads
//! no database, opens no socket and holds no state, so every edge case below
//! is a unit test rather than a live-session surprise.

use tickvault_common::types::ExchangeSegment;

/// Movers taken from EACH side — 37 gainers and 37 losers.
///
/// Operator, 2026-08-26: *"widen to top 37 gainers/losers each"*. The number
/// is arithmetic, not preference: 37 + 37 = 74 stocks at two contracts each is
/// 148 of the 150 slots on depth-20 sockets 3 to 5. 38 would need 152 and the
/// selection would be refused whole.
pub const MOVERS_PER_SIDE: usize = 37;

/// Stocks selected in total — 37 a side plus ONE more.
///
/// Operator, 2026-08-26: *"what about the remaining two dude we need to fill
/// this also dude with the help of top gainers or top losers"*.
///
/// 37 + 37 stocks at two contracts each is 148 of the 150 slots on depth-20
/// sockets 3 to 5, leaving two. Two slots is exactly one more stock, so a
/// 75th is taken — from whichever side moved harder — and the three sockets
/// fill to 150 with nothing stranded.
pub const MOVER_STOCKS_TOTAL: usize = MOVERS_PER_SIDE * 2 + 1;

/// Contracts the movers consume at an at-the-money call and put per stock.
pub const MOVER_CONTRACTS: usize = MOVER_STOCKS_TOTAL * 2;

/// One stock's measured move, as read from the candle frames.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StockMove {
    /// Dhan `SecurityId` of the UNDERLYING stock, not of a contract.
    pub security_id: u64,
    /// The stock's segment. Carried because `security_id` alone is not
    /// unique — the only unique key is `(security_id, exchange_segment)` per
    /// I-P1-11. Not theoretical: id 13 is NIFTY in `IDX_I` and an unrelated
    /// cash stock near ₹7,600 in `NSE_EQ`.
    pub segment: ExchangeSegment,
    /// `close_pct_from_prev_day` — the move against yesterday's close.
    pub pct_change: f64,
}

impl StockMove {
    /// The I-P1-11 composite identity.
    #[must_use]
    pub const fn key(&self) -> (u64, ExchangeSegment) {
        (self.security_id, self.segment)
    }
}

/// A ranked selection, plus what it had to refuse.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MoverSelection {
    /// Biggest risers, strongest first. At most [`MOVERS_PER_SIDE`].
    pub gainers: Vec<StockMove>,
    /// Biggest fallers, steepest first. At most [`MOVERS_PER_SIDE`].
    pub losers: Vec<StockMove>,
    /// The 75th stock, which fills the last two slots of sockets 3 to 5.
    ///
    /// Taken from whichever side moved HARDER — the next gainer or the next
    /// loser, by absolute move. Ranking it by magnitude rather than always
    /// preferring one side is what stops a strong-rally day from spending the
    /// slot on a stock that barely fell.
    ///
    /// `None` on a day too thin to have a 75th mover; the slots then sit
    /// unused rather than being filled with a flat stock.
    pub tiebreak: Option<StockMove>,
    /// Moves refused because the percentage was not a finite number.
    ///
    /// Counted rather than silently skipped: a rising count means the
    /// percentage stamping upstream is producing values it should not, and
    /// the ranking would otherwise absorb that without a word.
    pub refused_non_finite: usize,
    /// Stocks whose move was exactly zero.
    ///
    /// Excluded by design — see [`rank_movers`]. Counted because before 09:15
    /// EVERY stock is here, and a session where the count never falls is a
    /// session where the percentages never got computed.
    pub skipped_flat: usize,
    /// Duplicate `(security_id, segment)` entries dropped from the input.
    ///
    /// A stock ranked twice would take two of the 74 slots and subscribe the
    /// same two contracts twice, which Dhan answers with 804.
    pub dropped_duplicates: usize,
}

impl MoverSelection {
    /// Stocks selected in total — at most [`MOVER_STOCKS_TOTAL`].
    #[must_use]
    pub fn selected(&self) -> usize {
        self.gainers
            .len()
            .saturating_add(self.losers.len())
            .saturating_add(usize::from(self.tiebreak.is_some()))
    }

    /// Contracts these stocks imply at an at-the-money call and put each.
    #[must_use]
    pub fn contracts(&self) -> usize {
        self.selected().saturating_mul(2)
    }
}

/// Ranks today's moves into the top [`MOVERS_PER_SIDE`] risers and fallers.
///
/// # What is deliberately excluded
///
/// **A move of exactly zero is not a mover.** Padding the list with flat
/// stocks would fill a scarce depth socket with a book nobody asked to watch,
/// and — worse — it would make the ranking look full at 09:14 when nothing has
/// traded yet and every stock reads 0.00%. A short list is the honest answer
/// to a quiet market; `skipped_flat` says how short and why.
///
/// **A non-finite percentage is refused, not sorted.** `NaN` does not order,
/// so a single one can put a comparison-based sort into an inconsistent state
/// and produce an arbitrary ranking with no error anywhere.
///
/// # The overlap guard
///
/// With fewer than `2 × MOVERS_PER_SIDE` movers, the head and the tail of one
/// sorted list OVERLAP, and the same stock would appear as both a top gainer
/// and a top loser — taking two slots and double-subscribing its two
/// contracts, which Dhan answers with 804. The split is therefore taken from
/// one ordering with an explicit boundary, never as two independent takes.
///
/// # Ties
///
/// Broken by `(security_id, segment)`, ascending. Not cosmetic: an unstable
/// tie-break makes the selection differ between two runs over identical data,
/// so a socket would churn on a minute where nothing moved.
///
/// # Complexity
///
/// O(n log n) in the stocks supplied — a sort, and FLAGGED as such rather than
/// relabelled. n is the F&O stock universe, ~220, once a minute on the cold
/// path.
#[must_use]
pub fn rank_movers(moves: &[StockMove]) -> MoverSelection {
    let mut out = MoverSelection::default();
    let mut seen: std::collections::HashSet<(u64, ExchangeSegment)> =
        std::collections::HashSet::with_capacity(moves.len());
    let mut usable: Vec<StockMove> = Vec::with_capacity(moves.len());

    for m in moves {
        if !m.pct_change.is_finite() {
            out.refused_non_finite = out.refused_non_finite.saturating_add(1);
            continue;
        }
        if m.pct_change == 0.0 {
            out.skipped_flat = out.skipped_flat.saturating_add(1);
            continue;
        }
        if !seen.insert(m.key()) {
            out.dropped_duplicates = out.dropped_duplicates.saturating_add(1);
            continue;
        }
        usable.push(*m);
    }

    // Descending by move, then ascending by identity so ties are stable.
    // `total_cmp` rather than `partial_cmp`: every value here is already
    // finite, and `total_cmp` cannot return `None`, so there is no unwrap and
    // no arm that could silently order two prices as "equal".
    usable.sort_by(|a, b| {
        b.pct_change
            .total_cmp(&a.pct_change)
            .then_with(|| a.security_id.cmp(&b.security_id))
            .then_with(|| (a.segment as u8).cmp(&(b.segment as u8)))
    });

    // THE OVERLAP GUARD. With fewer than 74 movers the head and tail of this
    // one ordering meet, so the boundary is computed once and both sides are
    // cut from it — never two independent `take(37)` calls.
    // BALANCED when short, capped when not. `min(37, total)` first would be
    // wrong in a way that looks right: on a two-mover day it hands BOTH to the
    // gainers and leaves the losers empty, so a socket meant to watch the
    // day's worst faller watches nothing while the day plainly has one.
    // Splitting at the midpoint keeps both sides represented at every size,
    // and the cap still binds the moment the market is busy enough to fill
    // them.
    let total = usable.len();
    let gain_take = MOVERS_PER_SIDE.min(total.div_ceil(2));
    let lose_take = MOVERS_PER_SIDE.min(total.saturating_sub(gain_take));

    out.gainers = usable.iter().take(gain_take).copied().collect();
    // The tail, steepest fall first — so reversed off the end of the same
    // descending ordering.
    out.losers = usable
        .iter()
        .rev()
        .take(lose_take)
        .copied()
        .collect::<Vec<_>>();

    // THE 75TH STOCK — the last two slots of sockets 3 to 5.
    //
    // The candidates are the two entries the cuts above left behind: the next
    // one down from the gainers' cut, and the next one up from the losers'.
    // With the balanced split those are the same index whenever the day is
    // thin, so the bounds check below is what keeps this from re-selecting a
    // stock already taken — the overlap failure one step further along.
    let taken_head = gain_take;
    let taken_tail = total.saturating_sub(lose_take);
    if taken_head < taken_tail {
        // Whichever moved HARDER. Always preferring one side would spend the
        // slot on a stock that barely moved whenever the day is lopsided.
        let next_gainer = usable.get(taken_head).copied();
        let next_loser = usable.get(taken_tail.saturating_sub(1)).copied();
        out.tiebreak = match (next_gainer, next_loser) {
            (Some(g), Some(l)) if g.key() == l.key() => Some(g),
            (Some(g), Some(l)) => {
                if l.pct_change.abs() > g.pct_change.abs() {
                    Some(l)
                } else {
                    Some(g)
                }
            }
            (only, None) | (None, only) => only,
        };
    }

    out
}

#[cfg(test)]
mod tests {
    use super::{MOVER_CONTRACTS, MOVER_STOCKS_TOTAL, MOVERS_PER_SIDE, StockMove, rank_movers};
    use tickvault_common::types::ExchangeSegment;

    fn mv(id: u64, pct: f64) -> StockMove {
        StockMove {
            security_id: id,
            segment: ExchangeSegment::NseEquity,
            pct_change: pct,
        }
    }

    /// The number is arithmetic, not preference. 37 + 37 stocks at an
    /// at-the-money call and put each is 148 of the 150 slots on depth-20
    /// sockets 3 to 5. 38 would need 152 and the whole selection would be
    /// refused.
    #[test]
    fn thirty_seven_a_side_is_what_fits_the_three_sockets() {
        assert_eq!(MOVERS_PER_SIDE, 37);
        assert_eq!(MOVER_STOCKS_TOTAL, 75);
        assert_eq!(
            MOVER_CONTRACTS, 150,
            "37 + 37 + 1 stocks at two contracts each fills sockets 3 to 5 \
             EXACTLY — no slot stranded, none over"
        );
        assert!(
            (MOVERS_PER_SIDE + 1) * 2 * 2 > 150,
            "38 a side would fit, so 37 is not the ceiling and this constant \
             is under-selling the sockets"
        );
    }

    #[test]
    fn the_biggest_risers_and_steepest_fallers_come_back_in_order() {
        let moves: Vec<_> = (1..=100u64).map(|i| mv(i, i as f64 - 50.5)).collect();
        let out = rank_movers(&moves);

        assert_eq!(out.gainers.len(), 37);
        assert_eq!(out.losers.len(), 37);
        assert_eq!(out.gainers[0].security_id, 100, "strongest riser first");
        assert_eq!(out.losers[0].security_id, 1, "steepest faller first");
        assert!(out.gainers[0].pct_change > out.gainers[1].pct_change);
        assert!(out.losers[0].pct_change < out.losers[1].pct_change);
    }

    /// THE case that would double-subscribe. With fewer than 74 movers, two
    /// independent `take(37)` calls off one ordering overlap in the middle,
    /// and the same stock lands in both lists — two of the 74 slots spent on
    /// one name, and its two contracts subscribed twice, which Dhan answers
    /// with 804.
    #[test]
    fn a_thin_day_never_puts_the_same_stock_in_both_lists() {
        for count in 1..=80usize {
            let moves: Vec<_> = (1..=count as u64)
                .map(|i| mv(i, i as f64 - (count as f64 / 2.0) - 0.5))
                .collect();
            let out = rank_movers(&moves);

            let mut keys = std::collections::HashSet::new();
            for m in out
                .gainers
                .iter()
                .chain(out.losers.iter())
                .chain(out.tiebreak.iter())
            {
                assert!(
                    keys.insert(m.key()),
                    "stock {} appears in BOTH lists at {count} movers",
                    m.security_id
                );
            }
            assert!(out.selected() <= count, "selected more stocks than exist");
            assert!(out.selected() <= MOVER_STOCKS_TOTAL);
        }
    }

    /// A flat stock is not a mover. Before 09:15 every stock reads 0.00%, and
    /// padding the list with them would make the ranking look full while
    /// nothing had traded — filling three scarce depth sockets with books
    /// nobody asked to watch.
    #[test]
    fn a_market_that_has_not_opened_yet_selects_nobody() {
        let moves: Vec<_> = (1..=200u64).map(|i| mv(i, 0.0)).collect();
        let out = rank_movers(&moves);
        assert_eq!(out.selected(), 0);
        assert_eq!(out.skipped_flat, 200);
    }

    #[test]
    fn a_flat_stock_never_displaces_a_real_mover() {
        let mut moves: Vec<_> = (1..=50u64).map(|i| mv(i, 0.0)).collect();
        moves.push(mv(999, 1.5));
        moves.push(mv(998, -2.5));
        let out = rank_movers(&moves);

        assert_eq!(out.gainers.len(), 1);
        assert_eq!(out.losers.len(), 1);
        assert_eq!(out.gainers[0].security_id, 999);
        assert_eq!(out.losers[0].security_id, 998);
        assert_eq!(out.skipped_flat, 50);
    }

    /// `NaN` does not order. One of them inside a comparison sort can produce
    /// an arbitrary ranking with no error anywhere, so it is refused before
    /// the sort rather than sorted and hoped about.
    #[test]
    fn a_non_finite_percentage_is_refused_and_counted_not_ranked() {
        let moves = vec![
            mv(1, 5.0),
            mv(2, f64::NAN),
            mv(3, f64::INFINITY),
            mv(4, f64::NEG_INFINITY),
            mv(5, -5.0),
        ];
        let out = rank_movers(&moves);

        assert_eq!(out.refused_non_finite, 3);
        assert_eq!(out.gainers.len(), 1);
        assert_eq!(out.losers.len(), 1);
        assert_eq!(out.gainers[0].security_id, 1);
        assert_eq!(out.losers[0].security_id, 5);
    }

    /// A stock ranked twice takes two of the 74 slots and subscribes the same
    /// two contracts twice.
    #[test]
    fn the_same_stock_twice_takes_one_slot_and_is_counted() {
        let moves = vec![mv(7, 3.0), mv(7, 3.0), mv(7, 9.0), mv(8, -1.0)];
        let out = rank_movers(&moves);
        assert_eq!(out.dropped_duplicates, 2);
        assert_eq!(out.gainers.len(), 1);
        assert_eq!(
            out.gainers[0].pct_change, 3.0,
            "the FIRST reading of a stock wins, so the source decides which \
             row is authoritative rather than the sort order"
        );
    }

    /// `security_id` alone is not unique. Two segments sharing a number are
    /// two different instruments and both may legitimately rank.
    #[test]
    fn the_same_number_in_two_segments_is_two_stocks() {
        let moves = vec![
            StockMove {
                security_id: 13,
                segment: ExchangeSegment::NseEquity,
                pct_change: 4.0,
            },
            StockMove {
                security_id: 13,
                segment: ExchangeSegment::BseEquity,
                pct_change: 3.0,
            },
        ];
        let out = rank_movers(&moves);
        assert_eq!(
            out.dropped_duplicates, 0,
            "two segments sharing a number are two instruments, not a duplicate"
        );
        assert_eq!(
            out.selected(),
            2,
            "both must be SELECTED — which side of the balanced split they land \
             on is not the point of this test"
        );
    }

    /// An unstable tie-break makes the selection differ between two runs over
    /// identical data, so a socket would churn on a minute where nothing
    /// moved. Ties go to the lower id, every time.
    #[test]
    fn identical_moves_rank_deterministically() {
        let moves: Vec<_> = (1..=100u64).map(|i| mv(i, 2.0)).collect();
        let first = rank_movers(&moves);
        let mut shuffled = moves;
        shuffled.reverse();
        let second = rank_movers(&shuffled);
        assert_eq!(first.gainers, second.gainers);
        assert_eq!(first.losers, second.losers);
        assert_eq!(first.gainers[0].security_id, 1, "ties go to the lower id");
    }

    /// The last two slots of sockets 3 to 5. Operator, 2026-08-26:
    /// *"what about the remaining two dude we need to fill this also"*.
    #[test]
    fn the_seventy_fifth_stock_fills_the_last_two_slots() {
        let moves: Vec<_> = (1..=200u64).map(|i| mv(i, i as f64 - 100.5)).collect();
        let out = rank_movers(&moves);
        assert_eq!(out.gainers.len(), 37);
        assert_eq!(out.losers.len(), 37);
        assert!(out.tiebreak.is_some(), "the 75th stock was not selected");
        assert_eq!(out.selected(), 75);
        assert_eq!(out.contracts(), 150, "sockets 3 to 5 hold exactly 150");
    }

    /// The 75th comes from whichever side moved HARDER. Always preferring one
    /// side would spend the slot on a stock that barely moved whenever the
    /// day is lopsided — which is most days.
    #[test]
    fn the_seventy_fifth_goes_to_the_bigger_move_not_a_fixed_side() {
        // Losers far steeper than the gainers beyond the cut.
        let mut moves: Vec<_> = (1..=37u64)
            .map(|i| mv(i, 1.0 + i as f64 / 1000.0))
            .collect();
        moves.push(mv(500, 0.10)); // the next gainer: tiny
        moves.push(mv(600, -9.00)); // the next loser: huge
        moves.extend((1000..1037u64).map(|i| mv(i, -20.0 - i as f64 / 1000.0)));

        let out = rank_movers(&moves);
        let picked = out.tiebreak.expect("a 75th exists");
        assert_eq!(
            picked.security_id, 600,
            "the slot went to the weaker move — a fixed-side preference"
        );
    }

    /// A day too thin for a 75th leaves the two slots unused rather than
    /// filling them with a stock already subscribed, or a flat one.
    #[test]
    fn a_thin_day_leaves_the_last_two_slots_empty_rather_than_double_subscribing() {
        for count in 1..=80usize {
            let moves: Vec<_> = (1..=count as u64)
                .map(|i| mv(i, i as f64 - (count as f64 / 2.0) - 0.5))
                .collect();
            let out = rank_movers(&moves);

            let mut keys = std::collections::HashSet::new();
            for m in out
                .gainers
                .iter()
                .chain(out.losers.iter())
                .chain(out.tiebreak.iter())
            {
                assert!(
                    keys.insert(m.key()),
                    "stock {} selected TWICE at {count} movers — its two \
                     contracts would be subscribed twice, which Dhan answers \
                     with 804",
                    m.security_id
                );
            }
            assert!(out.selected() <= count);
            assert!(out.selected() <= MOVER_STOCKS_TOTAL);
            assert!(out.contracts() <= 150, "sockets 3 to 5 hold 150");
        }
    }

    #[test]
    fn an_empty_universe_is_an_empty_selection_not_a_panic() {
        let out = rank_movers(&[]);
        assert_eq!(out.selected(), 0);
        assert_eq!(out.contracts(), 0);
    }

    /// A day where every stock rose. Both sides still come from ONE ordering,
    /// so the "losers" are the weakest risers rather than a second take that
    /// could overlap the first.
    #[test]
    fn a_day_with_no_fallers_still_fills_both_sides_without_overlap() {
        let moves: Vec<_> = (1..=100u64).map(|i| mv(i, i as f64 / 10.0)).collect();
        let out = rank_movers(&moves);
        assert_eq!(out.gainers.len(), 37);
        assert_eq!(out.losers.len(), 37);
        assert!(out.losers[0].pct_change < out.gainers[36].pct_change);
        let mut keys = std::collections::HashSet::new();
        for m in out.gainers.iter().chain(out.losers.iter()) {
            assert!(keys.insert(m.key()), "overlap on an all-risers day");
        }
    }

    #[test]
    fn exactly_seventy_four_movers_fill_both_sides_with_nothing_left_over() {
        let moves: Vec<_> = (1..=74u64).map(|i| mv(i, i as f64 - 37.5)).collect();
        let out = rank_movers(&moves);
        assert_eq!(out.gainers.len(), 37);
        assert_eq!(out.losers.len(), 37);
        assert_eq!(out.selected(), 74);
        assert_eq!(out.contracts(), 148);
    }

    /// The whole F&O universe at once, so the cost is exercised at the real
    /// scale rather than at a test's scale.
    #[test]
    fn the_full_universe_ranks_without_panic_or_overlap() {
        let moves: Vec<_> = (1..=25_000u64)
            .map(|i| mv(i, ((i % 2_000) as f64) / 100.0 - 10.0))
            .collect();
        let out = rank_movers(&moves);
        assert_eq!(out.selected(), MOVER_STOCKS_TOTAL);
        let mut keys = std::collections::HashSet::new();
        for m in out
            .gainers
            .iter()
            .chain(out.losers.iter())
            .chain(out.tiebreak.iter())
        {
            assert!(keys.insert(m.key()));
        }
    }
}
