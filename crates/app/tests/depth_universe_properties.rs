//! Properties of the selector that decides which contracts get depth at all.
//!
//! # Why this surface
//!
//! `select_depth_universe` turns a chain snapshot into the two depth
//! subscription sets. It is the function that has already been wrong twice in
//! ways nobody could see from its call site: a linear scan inside a sort
//! comparator, and a raw-rupee ranking that let a 50-point gap on a 26,000
//! index outrank a 100-point gap on a 57,500 one — which on 26 August put
//! FINNIFTY and MIDCPNIFTY on four of the five 200-level sockets and left
//! **BANKNIFTY with none**, then tripped 322 redials on the sparse books it
//! chose.
//!
//! Both were found by reading output on a live day. These try to find the
//! next one before the market does.
//!
//! # The honest limit
//!
//! A passing run means no counterexample was found among the cases tried. The
//! input space is infinite; these widen the search rather than closing it.

use proptest::prelude::*;
use std::collections::BTreeSet;

use tickvault_app::dhan_depth_universe::{
    DEPTH_20_MAX_INSTRUMENTS, DEPTH_200_MAX_SOCKETS, DepthCandidate, select_depth_universe,
};
use tickvault_core::websocket::pool_supervisor::SubscribeInstrument;

/// Index underlyings only. Stocks are refused by this lane and have their own
/// counter, so mixing them in would spend most generated cases on a refusal
/// path that two hand-written tests already pin.
const UNDERLYINGS: [&str; 5] = ["NIFTY", "BANKNIFTY", "FINNIFTY", "MIDCPNIFTY", "SENSEX"];

/// Spot per underlying, at the real order of magnitude.
///
/// The magnitudes matter here in a way they rarely do: the ranking defect
/// this file exists to catch was invisible at equal spots and obvious at
/// 24,000 against 57,500. A generator that gave every underlying the same
/// spot could not have found it.
fn spot_for(u: &str) -> f64 {
    match u {
        "NIFTY" => 24_400.0,
        "BANKNIFTY" => 57_500.0,
        "FINNIFTY" => 26_200.0,
        "MIDCPNIFTY" => 12_600.0,
        _ => 81_000.0,
    }
}

fn key(i: &SubscribeInstrument) -> (u64, u8) {
    (i.security_id, i.segment as u8)
}

/// A contract id is a FUNCTION of contract identity: one `security_id` is one
/// contract, so it has one underlying, one strike and one leg. Drawing it
/// independently builds a chain no exchange can produce.
fn contract_id(u: usize, strike_step: i64, is_ce: bool, expiry_slot: i64) -> i64 {
    (((u as i64) * 64 + strike_step) * 3 + expiry_slot) * 2 + i64::from(is_ce) + 1
}

fn candidate() -> impl Strategy<Value = DepthCandidate> {
    (
        0_usize..UNDERLYINGS.len(),
        -8_i64..9,
        any::<bool>(),
        0_i64..3,
        // A spot that is sometimes unusable: the ranking divides by it.
        prop_oneof![
            9 => Just(1.0_f64),
            1 => Just(0.0_f64),
            1 => Just(f64::NAN),
        ],
    )
        .prop_map(|(u, step, is_ce, expiry_slot, spot_scale)| {
            let underlying = UNDERLYINGS[u];
            let base = spot_for(underlying);
            #[expect(clippy::cast_precision_loss, reason = "step is -8..9")]
            let strike = (step as f64).mul_add(base / 200.0, base);
            DepthCandidate {
                underlying: underlying.to_owned(),
                contract_security_id: contract_id(u, step, is_ce, expiry_slot),
                // Three expiries, so the nearest-expiry rule is exercised
                // rather than assumed away.
                expiry_micros: 1_900_000_000_000_000 + expiry_slot * 604_800_000_000,
                strike,
                spot: base * spot_scale,
                leg: if is_ce { "CE" } else { "PE" }.to_owned(),
                is_index_option: true,
            }
        })
}

fn chain() -> impl Strategy<Value = Vec<DepthCandidate>> {
    prop::collection::vec(candidate(), 0..120)
}

proptest! {
    /// THE OPERATOR'S BUDGET. Four 200-level sockets, 250 20-level slots.
    #[test]
    fn neither_depth_set_exceeds_its_authorized_budget(rows in chain()) {
        let got = select_depth_universe(&rows);
        prop_assert!(got.depth_200.len() <= DEPTH_200_MAX_SOCKETS);
        prop_assert!(got.depth_20.len() <= DEPTH_20_MAX_INSTRUMENTS);
    }

    /// NOTHING IS INVENTED. Every chosen instrument traces back to a row of
    /// the chain.
    ///
    /// A subscription to an id the chain never listed is a well-formed
    /// request that returns silence forever and looks exactly like a quiet
    /// book.
    #[test]
    fn every_chosen_contract_came_from_the_chain(rows in chain()) {
        let offered: BTreeSet<u64> = rows
            .iter()
            .filter(|c| c.contract_security_id > 0)
            .map(|c| c.contract_security_id as u64)
            .collect();
        let got = select_depth_universe(&rows);
        for inst in got.depth_20.iter().chain(got.depth_200.iter()) {
            prop_assert!(offered.contains(&inst.security_id), "invented {inst:?}");
        }
    }

    /// NO DUPLICATES WITHIN A SET (I-P1-11, on the composite key).
    ///
    /// A duplicate burns one of Dhan's fifty wire slots on that connection
    /// and inflates the count toward the 250 envelope, so real contracts get
    /// squeezed out by copies of ones already subscribed.
    #[test]
    fn no_contract_is_chosen_twice_in_either_set(rows in chain()) {
        let got = select_depth_universe(&rows);
        for set in [&got.depth_20, &got.depth_200] {
            let mut seen: BTreeSet<(u64, u8)> = BTreeSet::new();
            for i in set {
                prop_assert!(seen.insert(key(i)), "{:?} chosen twice", key(i));
            }
        }
    }

    /// CURRENT EXPIRY ONLY. No contract from a later expiry may be chosen
    /// while its underlying has a nearer one.
    ///
    /// A far month is an illiquid book, and the operator's rule is explicit.
    #[test]
    fn only_the_nearest_expiry_of_an_underlying_is_ever_chosen(rows in chain()) {
        let got = select_depth_universe(&rows);
        let chosen: BTreeSet<u64> = got
            .depth_20
            .iter()
            .chain(got.depth_200.iter())
            .map(|i| i.security_id)
            .collect();
        for c in &rows {
            if c.contract_security_id <= 0 || !chosen.contains(&(c.contract_security_id as u64)) {
                continue;
            }
            // Nearest among the rows that survive refusal. Since the spot
            // is decided per UNDERLYING rather than per row, a row is no
            // longer refused for carrying a stale copy of the price -- so
            // every row of an underlying that HAS a usable spot somewhere is
            // eligible, and the nearest expiry is the plain minimum over
            // them.
            let nearest = rows
                .iter()
                .filter(|o| o.underlying == c.underlying && o.contract_security_id > 0)
                .map(|o| o.expiry_micros)
                .min();
            prop_assert_eq!(
                Some(c.expiry_micros),
                nearest,
                "{} chose a far expiry",
                c.underlying
            );
        }
    }

    /// DEPTH-200 IS WHOLE PAIRS. The budget is even and a lone leg is
    /// retired, so an odd count means a pair was split.
    ///
    /// Two sockets that are not reading two sides of one book answer no
    /// cross-leg question at all — which is the entire reason a pair is the
    /// unit.
    #[test]
    fn depth_200_never_carries_a_half_pair(rows in chain()) {
        let got = select_depth_universe(&rows);
        prop_assert!(!got.depth_200_lone_leg, "a lone leg came back");
        prop_assert_eq!(got.depth_200.len() % 2, 0, "{:?}", got.depth_200);
    }

    /// THE OPERATOR'S 2026-08-26 LOCK. NIFTY and BANKNIFTY come first.
    ///
    /// If a chain offers a whole pair for a priority underlying, no
    /// non-priority underlying may hold a 200-level socket while that pair is
    /// unplaced. This is the exact miss that put FINNIFTY on four sockets and
    /// BANKNIFTY on none.
    #[test]
    fn a_priority_underlying_is_never_displaced_by_another(rows in chain()) {
        let got = select_depth_universe(&rows);
        if got.depth_200.is_empty() {
            return Ok(());
        }
        // Which underlying does each chosen id belong to?
        let owner = |id: u64| -> Option<&str> {
            rows.iter()
                .find(|c| c.contract_security_id > 0 && c.contract_security_id as u64 == id)
                .map(|c| c.underlying.as_str())
        };
        let chosen: Vec<&str> = got.depth_200.iter().filter_map(|i| owner(i.security_id)).collect();
        let has_non_priority = chosen
            .iter()
            .any(|u| *u != "NIFTY" && *u != "BANKNIFTY");
        if !has_non_priority {
            return Ok(());
        }
        // A non-priority underlying holds a socket. Then every priority
        // underlying that COULD have supplied a whole pair must already hold
        // one.
        for priority in ["NIFTY", "BANKNIFTY"] {
            let nearest = rows
                .iter()
                .filter(|c| c.underlying == priority && c.contract_security_id > 0)
                .map(|c| c.expiry_micros)
                .min();
            let Some(nearest) = nearest else { continue };
            let has_whole_pair = rows
                .iter()
                .filter(|c| {
                    c.underlying == priority
                        && c.expiry_micros == nearest
                        && c.contract_security_id > 0
                        && c.spot.is_finite()
                        && c.spot > 0.0
                })
                .any(|ce| {
                    ce.leg == "CE"
                        && rows.iter().any(|pe| {
                            pe.underlying == priority
                                && pe.expiry_micros == nearest
                                && pe.leg == "PE"
                                && pe.contract_security_id > 0
                                && (pe.strike - ce.strike).abs() < f64::EPSILON
                        })
                });
            if has_whole_pair {
                prop_assert!(
                    chosen.contains(&priority),
                    "{priority} had a whole pair and was displaced by {chosen:?}"
                );
            }
        }
    }

    /// AN UNDERLYING WITH NO PRICE AT ALL GETS NO SOCKET.
    ///
    /// The ranking divides by spot, so a zero or a NaN produces an ordering
    /// that is not an ordering and picks a contract that is arbitrary rather
    /// than at-the-money.
    ///
    /// Asked of the UNDERLYING, not of a row, and that is the whole point.
    /// Spot belongs to the underlying; the chain merely stamps a copy on every
    /// row, and a stale copy on one leg must not cost that contract its place.
    /// A price that is missing EVERYWHERE for an underlying is a different
    /// thing, and it still refuses.
    #[test]
    fn an_underlying_with_no_usable_price_gets_no_socket(rows in chain()) {
        let got = select_depth_universe(&rows);
        let chosen: BTreeSet<u64> = got
            .depth_20
            .iter()
            .chain(got.depth_200.iter())
            .map(|i| i.security_id)
            .collect();
        for id in &chosen {
            let Some(owner) = rows
                .iter()
                .find(|c| c.contract_security_id > 0 && c.contract_security_id as u64 == *id)
                .map(|c| c.underlying.as_str())
            else {
                continue;
            };
            let priced = rows
                .iter()
                .any(|c| c.underlying == owner && c.spot.is_finite() && c.spot > 0.0);
            prop_assert!(priced, "chose {id}: {owner} has no usable price on any row");
        }
    }

    /// DETERMINISTIC. The same chain gives the same answer twice.
    ///
    /// Two of the three maps in this function are `HashMap`s, whose iteration
    /// order is not stable even between runs of one process. Anything that
    /// leaks that order into the result would swap sockets on identical data.
    #[test]
    fn the_same_chain_selects_the_same_contracts_twice(rows in chain()) {
        let a = select_depth_universe(&rows);
        let b = select_depth_universe(&rows);
        prop_assert_eq!(
            a.depth_20.iter().map(key).collect::<Vec<_>>(),
            b.depth_20.iter().map(key).collect::<Vec<_>>()
        );
        prop_assert_eq!(
            a.depth_200.iter().map(key).collect::<Vec<_>>(),
            b.depth_200.iter().map(key).collect::<Vec<_>>()
        );
    }

    /// ORDER-INDEPENDENT. Reversing the chain rows cannot change WHICH
    /// contracts are chosen.
    ///
    /// The query carries no `ORDER BY`, so two minutes can legitimately
    /// return the same rows shuffled. A selection that depends on arrival
    /// order swaps sockets back and forth all day while reporting healthy
    /// activity the whole time.
    #[test]
    fn reversing_the_chain_does_not_change_the_chosen_set(rows in chain()) {
        let forward = select_depth_universe(&rows);
        let reversed: Vec<DepthCandidate> = rows.iter().rev().cloned().collect();
        let backward = select_depth_universe(&reversed);
        prop_assert_eq!(
            forward.depth_20.iter().map(key).collect::<BTreeSet<_>>(),
            backward.depth_20.iter().map(key).collect::<BTreeSet<_>>(),
            "depth-20 changed"
        );
        prop_assert_eq!(
            forward.depth_200.iter().map(key).collect::<BTreeSet<_>>(),
            backward.depth_200.iter().map(key).collect::<BTreeSet<_>>(),
            "depth-200 changed"
        );
    }

    /// Never panics. Float ordering, division by a generated spot and three
    /// interacting budgets are exactly where this function's defects lived.
    #[test]
    fn it_never_panics(rows in chain()) {
        let _ = select_depth_universe(&rows);
    }
}
