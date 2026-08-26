//! Properties of the selector that picks the ~24,000 contracts the main feed
//! subscribes.
//!
//! # Why this surface
//!
//! `select_contract_universe` is the operator's 2026-08-15 and 2026-08-21
//! scope in code: NIFTY and BANKNIFTY full current-expiry chains, every
//! futures expiry, and stock options at the money plus or minus twenty-five
//! on each side. Roughly twenty-two thousand of the ~24,600 subscribed
//! instruments come out of the stock-option window alone, and the whole set
//! has to fit five connections at five thousand each — `plan_pool` refuses
//! the WHOLE pool when it does not, so an overshoot here costs the session
//! its main feed rather than costing the excess.
//!
//! It is also a function whose reporting has already been wrong in the
//! dangerous direction: it printed `atm_window = 25` beside
//! `stock_options = 0`, a number describing a window applied to nothing.
//!
//! # The honest limit
//!
//! A passing run means no counterexample was found among the cases tried. The
//! input space is infinite; these widen the search rather than closing it.

use proptest::prelude::*;
use std::collections::{BTreeSet, HashMap};

use tickvault_app::dhan_contract_universe::{
    FULL_CHAIN_INDEX_UNDERLYINGS, select_contract_universe,
};
use tickvault_common::constants::STOCK_OPTION_ATM_STRIKES_EACH_SIDE;
use tickvault_core::instrument::master_csv::{InstrumentClass, MasterRow, OptionLeg};

/// Two index underlyings that take a full chain, and three stocks that take
/// the window. Both halves of the operator's rule are then exercised by every
/// generated case rather than by whichever the generator happened to pick.
const UNDERLYINGS: [&str; 5] = ["NIFTY", "BANKNIFTY", "RELIANCE", "TCS", "INFY"];

/// 26 August 2026, packed `YYYYMMDD` — the master's own encoding, where
/// numeric and calendar order coincide.
#[expect(
    clippy::inconsistent_digit_grouping,
    reason = "YYYY_MM_DD reads as a date"
)]
const TODAY: u32 = 2026_08_26;

fn base_paise(u: &str) -> i64 {
    match u {
        "NIFTY" => 2_440_000,
        "BANKNIFTY" => 5_750_000,
        "RELIANCE" => 140_000,
        "TCS" => 310_000,
        _ => 150_000,
    }
}

fn row(
    security_id: u64,
    class: InstrumentClass,
    underlying: &str,
    expiry_ymd: u32,
    strike_paise: i64,
    leg: OptionLeg,
) -> MasterRow {
    MasterRow {
        security_id,
        isin: String::new(),
        symbol_name: format!("{underlying}-{security_id}"),
        exch_id: "NSE".to_owned(),
        segment: "D".to_owned(),
        series: String::new(),
        class,
        expiry_ymd,
        strike_paise,
        option_leg: leg,
        underlying_symbol: underlying.to_owned(),
    }
}

/// A master built the way the real one is shaped: several expiries per
/// underlying, a ladder of strikes on each, both legs, plus futures.
fn master() -> impl Strategy<Value = Vec<MasterRow>> {
    (
        prop::collection::vec(0_usize..UNDERLYINGS.len(), 1..6),
        1_usize..40,
        prop::collection::vec(0_u32..3, 1..4),
    )
        .prop_map(|(picked, strikes_each_side, expiry_offsets)| {
            let mut rows = Vec::new();
            let mut id = 1_u64;
            let mut seen: BTreeSet<usize> = BTreeSet::new();
            for u in picked {
                if !seen.insert(u) {
                    continue;
                }
                let underlying = UNDERLYINGS[u];
                let base = base_paise(underlying);
                let is_index = FULL_CHAIN_INDEX_UNDERLYINGS.contains(&underlying);
                for off in &expiry_offsets {
                    // Expiries within the same month, so YYYYMMDD ordering is
                    // plain integer ordering and the nearest is unambiguous.
                    let expiry = TODAY + off;
                    let step = base / 100;
                    for k in -(strikes_each_side as i64)..=(strikes_each_side as i64) {
                        let strike = base + k * step.max(1);
                        for leg in [OptionLeg::Call, OptionLeg::Put] {
                            rows.push(row(
                                id,
                                if is_index {
                                    InstrumentClass::IndexOption
                                } else {
                                    InstrumentClass::StockOption
                                },
                                underlying,
                                expiry,
                                strike,
                                leg,
                            ));
                            id += 1;
                        }
                    }
                    rows.push(row(
                        id,
                        if is_index {
                            InstrumentClass::IndexFuture
                        } else {
                            InstrumentClass::StockFuture
                        },
                        underlying,
                        expiry,
                        0,
                        OptionLeg::None,
                    ));
                    id += 1;
                }
            }
            rows
        })
}

/// Spot prices for a subset of the underlyings, so the no-price path — the
/// pre-open shape, and the mid-session unreachable-source shape — is a normal
/// generated case rather than an afterthought.
fn spots() -> impl Strategy<Value = HashMap<String, i64>> {
    prop::collection::vec((0_usize..UNDERLYINGS.len(), 0_u8..3), 0..6).prop_map(|picks| {
        let mut out = HashMap::new();
        for (u, kind) in picks {
            let underlying = UNDERLYINGS[u];
            let price = match kind {
                // A real price, a zero, and a negative: the last two must be
                // refused rather than used to centre a window.
                0 => base_paise(underlying),
                1 => 0,
                _ => -1,
            };
            out.insert(underlying.to_owned(), price);
        }
        out
    })
}

proptest! {
    /// THE CAPACITY CEILING IS NEVER EXCEEDED.
    ///
    /// `plan_pool` refuses the WHOLE main-feed pool when the set does not fit
    /// five connections, so an overshoot here does not cost the excess — it
    /// costs the session its price feed.
    #[test]
    fn the_selection_never_exceeds_the_capacity_it_was_given(
        rows in master(),
        spot in spots(),
        capacity in 0_usize..3000,
    ) {
        let got = select_contract_universe(&rows, &spot, TODAY, capacity);
        prop_assert!(
            got.instruments.len() <= capacity,
            "{} instruments for a capacity of {capacity}",
            got.instruments.len()
        );
    }

    /// NOTHING IS INVENTED, AND NOTHING IS SUBSCRIBED TWICE.
    ///
    /// An id the master never listed is a well-formed subscribe that returns
    /// silence forever; a duplicate is what Dhan answers with 804 — Fatal,
    /// and the connection is gone for the session.
    #[test]
    fn every_instrument_is_from_the_master_and_appears_once(
        rows in master(),
        spot in spots(),
        capacity in 0_usize..3000,
    ) {
        let offered: BTreeSet<u64> = rows.iter().map(|r| r.security_id).collect();
        let got = select_contract_universe(&rows, &spot, TODAY, capacity);
        let mut seen: BTreeSet<(u64, u8)> = BTreeSet::new();
        for inst in &got.instruments {
            prop_assert!(offered.contains(&inst.security_id), "invented {inst:?}");
            prop_assert!(
                seen.insert((inst.security_id, inst.segment as u8)),
                "{inst:?} selected twice"
            );
        }
    }

    /// CURRENT EXPIRY ONLY, FOR OPTIONS.
    ///
    /// Options take the nearest expiry and nothing else — the operator's rule,
    /// and the difference between a liquid book and a dead one. Futures take
    /// every expiry by the same rule, so this asks only about options.
    #[test]
    fn no_option_from_a_later_expiry_is_ever_selected(
        rows in master(),
        spot in spots(),
        capacity in 0_usize..3000,
    ) {
        let got = select_contract_universe(&rows, &spot, TODAY, capacity);
        let chosen: BTreeSet<u64> = got.instruments.iter().map(|i| i.security_id).collect();
        for r in &rows {
            if !r.class.is_option() || !chosen.contains(&r.security_id) {
                continue;
            }
            let nearest = rows
                .iter()
                .filter(|o| {
                    o.class.is_option()
                        && o.underlying_symbol == r.underlying_symbol
                        && o.expiry_ymd >= TODAY
                })
                .map(|o| o.expiry_ymd)
                .min();
            prop_assert_eq!(
                Some(r.expiry_ymd),
                nearest,
                "{} chose a far expiry",
                r.underlying_symbol
            );
        }
    }

    /// THE WINDOW IS NEVER WIDER THAN THE OPERATOR ASKED FOR.
    ///
    /// Twenty-five each side is a ceiling, not a target. It may shrink when
    /// the envelope is tight — that is the design — but it may never widen,
    /// because the instruments beyond it were never authorised.
    #[test]
    fn the_atm_window_never_exceeds_the_authorised_twenty_five(
        rows in master(),
        spot in spots(),
        capacity in 0_usize..3000,
    ) {
        let got = select_contract_universe(&rows, &spot, TODAY, capacity);
        prop_assert!(got.atm_window_used <= STOCK_OPTION_ATM_STRIKES_EACH_SIDE);
    }

    /// THE REPORTED WINDOW DESCRIBES SOMETHING THAT HAPPENED.
    ///
    /// This function once printed `atm_window = 25` beside
    /// `stock_options = 0` — a number describing a window applied to nothing,
    /// which reads as a healthy selection to anyone watching the boot line.
    /// A non-zero window must mean stock options were actually chosen, and a
    /// zero window must carry a reason that says which of the two failures it
    /// was.
    #[test]
    fn a_reported_window_is_never_a_window_applied_to_nothing(
        rows in master(),
        spot in spots(),
        capacity in 0_usize..3000,
    ) {
        let got = select_contract_universe(&rows, &spot, TODAY, capacity);
        if got.atm_window_used > 0 {
            prop_assert!(
                got.stock_options > 0,
                "window {} applied to {} stock options",
                got.atm_window_used,
                got.stock_options
            );
            prop_assert_eq!(got.atm_window_reason, "applied");
        } else {
            prop_assert!(
                matches!(got.atm_window_reason, "no_room" | "no_ladders"),
                "a zero window with reason {:?}",
                got.atm_window_reason
            );
        }
    }

    /// AN UNPRICED UNDERLYING GETS NO OPTIONS.
    ///
    /// The window is centred on spot. With no usable price there is nothing to
    /// centre on, and a window centred on a guess subscribes the wrong strikes
    /// and reads as a quiet book. Futures are unaffected — they need no spot.
    #[test]
    fn a_stock_with_no_usable_price_contributes_no_options(
        rows in master(),
        spot in spots(),
        capacity in 0_usize..3000,
    ) {
        let got = select_contract_universe(&rows, &spot, TODAY, capacity);
        let chosen: BTreeSet<u64> = got.instruments.iter().map(|i| i.security_id).collect();
        for r in &rows {
            if r.class != InstrumentClass::StockOption || !chosen.contains(&r.security_id) {
                continue;
            }
            let priced = spot
                .get(&r.underlying_symbol)
                .copied()
                .is_some_and(|p| p > 0);
            prop_assert!(priced, "{} options chosen with no usable spot", r.underlying_symbol);
        }
    }

    /// BOTH LEGS OR NEITHER, for the stock-option window.
    ///
    /// A strike inside the window contributes its call AND its put. Half a
    /// strike is a book whose other side nobody is reading, and every
    /// cross-leg quantity computed from it is wrong rather than missing.
    #[test]
    fn a_windowed_strike_contributes_both_of_its_legs(
        rows in master(),
        spot in spots(),
        capacity in 0_usize..3000,
    ) {
        let got = select_contract_universe(&rows, &spot, TODAY, capacity);
        let chosen: BTreeSet<u64> = got.instruments.iter().map(|i| i.security_id).collect();
        // Group the chosen stock-option rows by (underlying, expiry, strike).
        let mut by_strike: HashMap<(&str, u32, i64), (bool, bool)> = HashMap::new();
        for r in &rows {
            if r.class != InstrumentClass::StockOption || !chosen.contains(&r.security_id) {
                continue;
            }
            let slot = by_strike
                .entry((r.underlying_symbol.as_str(), r.expiry_ymd, r.strike_paise))
                .or_insert((false, false));
            match r.option_leg {
                OptionLeg::Call => slot.0 = true,
                OptionLeg::Put => slot.1 = true,
                OptionLeg::None => {}
            }
        }
        for (k, (ce, pe)) in by_strike {
            prop_assert!(ce && pe, "{k:?} selected as a half strike");
        }
    }

    /// DETERMINISTIC. The same master and the same prices select the same
    /// contracts twice.
    ///
    /// The bucketing is done with `HashMap`s, whose iteration order is not
    /// stable between runs of one process. Anything that leaks that order into
    /// the chosen set would change the subscription on identical input.
    #[test]
    fn the_same_master_selects_the_same_contracts_twice(
        rows in master(),
        spot in spots(),
        capacity in 0_usize..3000,
    ) {
        let a = select_contract_universe(&rows, &spot, TODAY, capacity);
        let b = select_contract_universe(&rows, &spot, TODAY, capacity);
        prop_assert_eq!(
            a.instruments.iter().map(|i| (i.security_id, i.segment as u8)).collect::<Vec<_>>(),
            b.instruments.iter().map(|i| (i.security_id, i.segment as u8)).collect::<Vec<_>>()
        );
        prop_assert_eq!(a.atm_window_used, b.atm_window_used);
    }

    /// ORDER-INDEPENDENT. Reversing the master rows cannot change WHICH
    /// contracts are chosen.
    ///
    /// A master is a file, and a file's row order is not a contract. A
    /// selection that depends on it would change the subscription on a day the
    /// vendor merely re-sorted the export.
    #[test]
    fn reversing_the_master_does_not_change_the_chosen_set(
        rows in master(),
        spot in spots(),
        capacity in 0_usize..3000,
    ) {
        let forward = select_contract_universe(&rows, &spot, TODAY, capacity);
        let reversed: Vec<MasterRow> = rows.iter().rev().cloned().collect();
        let backward = select_contract_universe(&reversed, &spot, TODAY, capacity);
        prop_assert_eq!(
            forward.instruments.iter().map(|i| (i.security_id, i.segment as u8)).collect::<BTreeSet<_>>(),
            backward.instruments.iter().map(|i| (i.security_id, i.segment as u8)).collect::<BTreeSet<_>>()
        );
    }

    /// Never panics. A generated ladder, saturating paise arithmetic, an
    /// envelope that can be zero, and four interacting buckets.
    #[test]
    fn it_never_panics(rows in master(), spot in spots(), capacity in 0_usize..3000) {
        let _ = select_contract_universe(&rows, &spot, TODAY, capacity);
    }
}
