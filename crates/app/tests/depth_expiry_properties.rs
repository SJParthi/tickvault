//! Properties the current-expiry rule must hold for EVERY chain.
//!
//! # Why
//!
//! Operator, 2026-08-26: *"always current expiry alone only ... especially
//! for depth 20 and depth 200"*, and separately *"for nifty always weekly
//! current expiry and remaining entirely only monthly"*.
//!
//! Seven hand-written permutations cover the shapes I thought of. A real
//! chain is assembled by a vendor, so the shapes it produces are not the
//! ones I would choose: strikes listed out of order, a leg missing on one
//! month and present on another, several expiries interleaved, the same
//! strike repeated, an underlying whose rows all carry the missing-expiry
//! sentinel. The generator reaches those; enumeration does not.
//!
//! # The strongest property here
//!
//! `every_emitted_contract_came_from_the_input` — the planner may drop, but
//! it may never INVENT an id. A fabricated `security_id` is a subscription
//! that returns silence forever and looks perfectly healthy while doing it,
//! which is the failure this whole subsystem is shaped to avoid.
//!
//! # The honest limit
//!
//! A passing run means no counterexample was found among the cases tried.
//! The input space is infinite. These widen the search; they do not close
//! it.

use proptest::prelude::*;
use std::collections::{BTreeMap, BTreeSet};

use tickvault_app::depth_rebalance::{atm_pair_for, chain_minutes_from_candidates};
use tickvault_app::dhan_depth_universe::DepthCandidate;

/// Three underlyings, so per-underlying independence is actually exercised.
const UNDERLYINGS: [&str; 3] = ["NIFTY", "BANKNIFTY", "RELIANCE"];

/// A small set of expiries INCLUDING the two sentinels — zero and negative —
/// because both mean "unknown month" and neither may ever rank as nearest.
const EXPIRIES: [i64; 5] = [
    0,
    -5,
    1_900_000_000_000_000,
    1_901_000_000_000_000,
    1_902_000_000_000_000,
];

fn candidate() -> impl Strategy<Value = DepthCandidate> {
    (
        0_usize..UNDERLYINGS.len(),
        0_usize..EXPIRIES.len(),
        // A tiny strike range so repeats and near-misses actually occur.
        0_i64..6,
        any::<bool>(),
        1_i64..40,
        any::<bool>(),
    )
        .prop_map(|(u, e, k, is_ce, id, index)| {
            #[expect(clippy::cast_precision_loss, reason = "k is 0..6")]
            let strike = (k as f64).mul_add(50.0, 24_400.0);
            DepthCandidate {
                underlying: UNDERLYINGS[u].to_owned(),
                contract_security_id: id,
                expiry_micros: EXPIRIES[e],
                strike,
                spot: 24_500.0,
                leg: if is_ce { "CE" } else { "PE" }.to_owned(),
                is_index_option: index,
            }
        })
}

fn chain() -> impl Strategy<Value = Vec<DepthCandidate>> {
    prop::collection::vec(candidate(), 0..30)
}

/// The nearest expiry an underlying has, by the rule under test: only
/// positive values count as known.
fn nearest_for(rows: &[DepthCandidate], underlying: &str) -> Option<i64> {
    rows.iter()
        .filter(|c| c.underlying == underlying && c.expiry_micros > 0)
        .map(|c| c.expiry_micros)
        .min()
}

proptest! {
    /// THE RULE. Every contract that reaches a depth socket belongs to its
    /// own underlying's nearest expiry — never a far month, never one of the
    /// unknown-month sentinels.
    #[test]
    fn every_emitted_contract_is_on_its_underlyings_nearest_expiry(rows in chain()) {
        let minutes = chain_minutes_from_candidates(&rows);
        for minute in &minutes {
            let nearest = nearest_for(&rows, &minute.underlying);
            // id -> the set of expiries that id appears under in the input.
            let mut expiries_of: BTreeMap<i64, BTreeSet<i64>> = BTreeMap::new();
            for c in rows.iter().filter(|c| c.underlying == minute.underlying) {
                expiries_of
                    .entry(c.contract_security_id)
                    .or_default()
                    .insert(c.expiry_micros);
            }
            for pair in &minute.pairs {
                for id in [pair.ce_security_id, pair.pe_security_id] {
                    let seen = expiries_of.get(&id).cloned().unwrap_or_default();
                    match nearest {
                        Some(n) => prop_assert!(
                            seen.contains(&n),
                            "{} emitted id {id}, which never appears under the nearest \
                             expiry {n} (it appears under {seen:?})",
                            minute.underlying
                        ),
                        // No dated row at all: the sentinels are all this
                        // underlying has, and refusing them would drop it.
                        None => prop_assert!(!seen.is_empty()),
                    }
                }
            }
        }
    }

    /// The planner may DROP a contract. It may never INVENT one. A fabricated
    /// id subscribes an instrument that returns silence forever while looking
    /// healthy.
    #[test]
    fn every_emitted_contract_came_from_the_input(rows in chain()) {
        let known: BTreeSet<i64> = rows.iter().map(|c| c.contract_security_id).collect();
        for minute in chain_minutes_from_candidates(&rows) {
            for pair in &minute.pairs {
                prop_assert!(known.contains(&pair.ce_security_id), "invented {pair:?}");
                prop_assert!(known.contains(&pair.pe_security_id), "invented {pair:?}");
            }
        }
        for u in UNDERLYINGS {
            if let Some(pair) = atm_pair_for(&rows, u) {
                prop_assert!(known.contains(&pair.ce_security_id), "invented {pair:?}");
                prop_assert!(known.contains(&pair.pe_security_id), "invented {pair:?}");
            }
        }
    }

    /// A strike reaches a socket only with BOTH legs. The tracker subscribes
    /// a CALL socket and a PUT socket per underlying; a half-filled strike
    /// strands one of them on a different strike, so the two are no longer
    /// reading the same book.
    #[test]
    fn a_strike_never_reaches_a_socket_with_one_leg(rows in chain()) {
        for minute in chain_minutes_from_candidates(&rows) {
            for pair in &minute.pairs {
                prop_assert!(pair.ce_security_id > 0, "{pair:?}");
                prop_assert!(pair.pe_security_id > 0, "{pair:?}");
                prop_assert_ne!(
                    pair.ce_security_id, pair.pe_security_id,
                    "one contract used as both legs: {:?}", pair
                );
            }
        }
    }

    /// A strike appears at most once per underlying. Two entries for one
    /// strike would spend two depth slots on one book.
    #[test]
    fn no_strike_is_emitted_twice_for_one_underlying(rows in chain()) {
        for minute in chain_minutes_from_candidates(&rows) {
            let mut strikes: Vec<i64> = minute.pairs.iter().map(|p| p.strike_paise).collect();
            let before = strikes.len();
            strikes.sort_unstable();
            strikes.dedup();
            prop_assert_eq!(strikes.len(), before, "{} emitted a strike twice", minute.underlying);
        }
    }

    /// THE TWO READERS AGREE. `atm_pair_for` and the chain view answer
    /// different questions about the same chain; if they disagreed about
    /// which month is current, a socket would hold a month nothing else is
    /// looking at.
    #[test]
    fn the_atm_search_and_the_chain_view_never_disagree_on_the_month(rows in chain()) {
        for u in UNDERLYINGS {
            let Some(pair) = atm_pair_for(&rows, u) else { continue };
            let nearest = nearest_for(&rows, u);
            let Some(n) = nearest else { continue };
            for id in [pair.ce_security_id, pair.pe_security_id] {
                let on_nearest = rows.iter().any(|c| {
                    c.underlying == u && c.contract_security_id == id && c.expiry_micros == n
                });
                prop_assert!(
                    on_nearest,
                    "the at-the-money pick {id} for {u} is not on the nearest expiry {n}"
                );
            }
        }
    }

    /// Input order must not decide anything. A vendor chain arrives in
    /// whatever order it arrives in.
    #[test]
    fn reversing_the_input_does_not_change_the_result(rows in chain()) {
        let forward = chain_minutes_from_candidates(&rows);
        let backward: Vec<DepthCandidate> = rows.iter().rev().cloned().collect();
        let reversed = chain_minutes_from_candidates(&backward);
        // Compare as SETS per underlying: emission order is not part of the
        // contract, but the content is.
        let key = |ms: &[tickvault_app::depth_rebalance::OwnedChainMinute]| {
            ms.iter()
                .map(|m| {
                    let mut ids: Vec<(i64, i64, i64)> = m
                        .pairs
                        .iter()
                        .map(|p| (p.strike_paise, p.ce_security_id, p.pe_security_id))
                        .collect();
                    ids.sort_unstable();
                    (m.underlying.clone(), ids)
                })
                .collect::<BTreeSet<_>>()
        };
        prop_assert_eq!(key(&forward), key(&reversed));
    }

    /// Determinism, and never a panic. Float strikes, sentinel expiries and
    /// two independently-sized collections are exactly where this file's
    /// sibling module hid three defects.
    #[test]
    fn it_is_deterministic_and_never_panics(rows in chain()) {
        let a = chain_minutes_from_candidates(&rows);
        let b = chain_minutes_from_candidates(&rows);
        prop_assert_eq!(a.len(), b.len());
        for u in UNDERLYINGS {
            prop_assert_eq!(atm_pair_for(&rows, u), atm_pair_for(&rows, u));
        }
    }
}
