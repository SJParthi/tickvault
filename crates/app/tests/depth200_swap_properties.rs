//! Properties of the actuator that turns an at-the-money decision into wire
//! traffic.
//!
//! # Why this surface
//!
//! `plan_swaps` is the last step before a live depth-200 connection is told
//! to drop one contract and take another. Everything upstream can be right
//! and still do damage here: a swap naming the wrong socket subscribes a
//! perfectly valid contract to the wrong connection and nothing downstream
//! can tell; a swap whose halves disagree unsubscribes one contract and
//! subscribes an unrelated one; and two sockets of one endpoint told to take
//! the same instrument is a duplicate subscribe, which Dhan answers with 804
//! — Fatal, connection gone for the session.
//!
//! It also has to do NOTHING on an ordinary minute. The tracker is
//! edge-triggered precisely so a session costs zero wire traffic when neither
//! strike moved; a version that emitted a no-op swap each minute would send
//! ~1,500 needless messages a session while every counter read as healthy
//! activity.
//!
//! # The honest limit
//!
//! A passing run means no counterexample was found among the cases tried. The
//! input space is infinite; these widen the search rather than closing it.

use proptest::prelude::*;
use std::collections::BTreeSet;

use tickvault_app::depth200_atm::{
    ChainMinute, DEPTH_200_ATM_SOCKETS, DEPTH_200_ATM_UNDERLYINGS, Depth200AtmConfig,
    Depth200AtmTracker, StrikePair, plan_swaps,
};
use tickvault_common::types::ExchangeSegment;
use tickvault_core::websocket::pool_supervisor::SubscribeInstrument;

/// Ladder start, in paise: 24,000.00 to 24,100.00 rupees.
const LADDER_BASE_LO: i64 = 2_400_000;
const LADDER_BASE_HI: i64 = 2_410_000;

/// Contract ids are a function of (underlying, strike, leg), so one id is one
/// contract — and the two underlyings draw from DISJOINT id ranges, which is
/// what a real exchange does. A generator that let NIFTY and BANKNIFTY share
/// an id would be asking whether the code survives corrupt vendor data, which
/// is a different question from whether it is correct on good data, and
/// mixing the two makes a failure ambiguous.
fn ladder(base_paise: i64, count: usize, underlying_slot: i64, id_epoch: i64) -> Vec<StrikePair> {
    (0..count)
        .map(|k| {
            let strike = base_paise + (k as i64) * 5_000;
            let base_id = underlying_slot * 10_000_000 + strike * 4 + id_epoch * 2;
            StrikePair {
                strike_paise: strike,
                ce_security_id: base_id + 1,
                pe_security_id: base_id + 2,
            }
        })
        .collect()
}

/// What the four sockets are believed to hold, in dial order: NIFTY call,
/// NIFTY put, BANKNIFTY call, BANKNIFTY put.
fn held() -> Vec<SubscribeInstrument> {
    (0..DEPTH_200_ATM_SOCKETS)
        .map(|i| SubscribeInstrument {
            security_id: 900_000 + i as u64,
            segment: ExchangeSegment::NseFno,
        })
        .collect()
}

fn segment_for(_underlying: &str) -> Option<ExchangeSegment> {
    Some(ExchangeSegment::NseFno)
}

fn spot() -> impl Strategy<Value = f64> {
    24_000.0_f64..25_600.0
}

proptest! {
    /// THE OLD HALF OF A SWAP IS READ FROM THE SOCKET, NEVER ASSUMED.
    ///
    /// A depth-200 connection holds exactly one instrument, and the
    /// unsubscribe is sent first. If the `old` named a contract the socket
    /// does not hold, the unsubscribe is a no-op and the subscribe then asks
    /// for a second instrument on that connection — 804, Fatal.
    #[test]
    fn the_old_half_of_every_swap_is_what_the_socket_holds(
        base in LADDER_BASE_LO..LADDER_BASE_HI,
        count in 1_usize..20,
        spots in prop::collection::vec(spot(), 1..12),
    ) {
        let nifty = ladder(base, count, 0, 0);
        let bank = ladder(base, count, 1, 0);
        let mut tracker = Depth200AtmTracker::new(Depth200AtmConfig::default());
        let mut sockets = held();
        for s in spots {
            let minutes = [
                ChainMinute { underlying: DEPTH_200_ATM_UNDERLYINGS[0], spot: s, pairs: &nifty },
                ChainMinute { underlying: DEPTH_200_ATM_UNDERLYINGS[1], spot: s, pairs: &bank },
            ];
            let swaps = plan_swaps(&mut tracker, &minutes, &sockets, segment_for);
            for swap in &swaps {
                prop_assert_eq!(
                    Some(swap.old),
                    sockets.get(swap.socket_index).copied(),
                    "swap named an `old` the socket does not hold"
                );
            }
            // Apply them, the way the caller does, so the next minute is
            // planned against what the sockets now really hold.
            for swap in &swaps {
                if let Some(slot) = sockets.get_mut(swap.socket_index) {
                    *slot = swap.new;
                }
            }
        }
    }

    /// NO SWAP IS A NO-OP.
    ///
    /// Wire traffic that changes nothing is not free: a depth-200 unsubscribe
    /// and re-subscribe drops the book for the round trip, and the counters
    /// would read as ordinary steering activity while nothing steered.
    #[test]
    fn a_swap_never_replaces_a_contract_with_itself(
        base in LADDER_BASE_LO..LADDER_BASE_HI,
        count in 1_usize..20,
        spots in prop::collection::vec(spot(), 1..12),
    ) {
        let nifty = ladder(base, count, 0, 0);
        let mut tracker = Depth200AtmTracker::new(Depth200AtmConfig::default());
        let mut sockets = held();
        for s in spots {
            let minutes = [ChainMinute {
                underlying: DEPTH_200_ATM_UNDERLYINGS[0],
                spot: s,
                pairs: &nifty,
            }];
            let swaps = plan_swaps(&mut tracker, &minutes, &sockets, segment_for);
            for swap in &swaps {
                prop_assert_ne!(swap.old, swap.new, "a swap that changes nothing");
                if let Some(slot) = sockets.get_mut(swap.socket_index) {
                    *slot = swap.new;
                }
            }
        }
    }

    /// A RE-ATTACH ONTO WHAT IS ALREADY HELD COSTS NOTHING.
    ///
    /// After a reconnect the tracker starts empty while the sockets still
    /// hold the at-the-money pair, so the next minute is a FIRST ADOPTION of
    /// contracts already subscribed. Without the equality guard that emits
    /// four swaps: each unsubscribes a contract and re-subscribes the same
    /// one, dropping four order books for the round trip and reporting the
    /// churn as steering activity.
    ///
    /// **This property exists because breaking the guard on purpose failed
    /// nothing.** The two other no-op properties could not reach the case:
    /// the tracker is edge-triggered, so a switch always carries a different
    /// strike or a different contract id, and the only way `new == old` is a
    /// tracker that has forgotten what the socket still holds. That is a
    /// reconnect, which is ordinary, and no test was producing one.
    #[test]
    fn a_reattach_onto_the_contract_already_held_produces_no_swap(
        base in LADDER_BASE_LO..LADDER_BASE_HI,
        count in 1_usize..20,
        s in spot(),
    ) {
        let nifty = ladder(base, count, 0, 0);
        let bank = ladder(base, count, 1, 0);
        let minutes = [
            ChainMinute { underlying: DEPTH_200_ATM_UNDERLYINGS[0], spot: s, pairs: &nifty },
            ChainMinute { underlying: DEPTH_200_ATM_UNDERLYINGS[1], spot: s, pairs: &bank },
        ];
        let mut sockets = held();
        let mut tracker = Depth200AtmTracker::new(Depth200AtmConfig::default());
        for swap in plan_swaps(&mut tracker, &minutes, &sockets, segment_for) {
            if let Some(slot) = sockets.get_mut(swap.socket_index) {
                *slot = swap.new;
            }
        }
        // The reconnect: tracker state gone, sockets unchanged.
        let mut reattached = Depth200AtmTracker::new(Depth200AtmConfig::default());
        let swaps = plan_swaps(&mut reattached, &minutes, &sockets, segment_for);
        prop_assert!(
            swaps.is_empty(),
            "re-attach churned {} sockets it was already holding: {swaps:?}",
            swaps.len()
        );
    }

    /// ONE SOCKET, ONE SWAP PER MINUTE — and never the same instrument onto
    /// two sockets.
    ///
    /// Two swaps for one socket in a minute means the second undoes the
    /// first. Two sockets told to take the SAME instrument is a duplicate
    /// subscribe across two connections of one endpoint, which is the 804
    /// case with the blast radius doubled.
    #[test]
    fn one_minute_never_targets_a_socket_twice_or_duplicates_an_instrument(
        base in LADDER_BASE_LO..LADDER_BASE_HI,
        count in 1_usize..20,
        spots in prop::collection::vec(spot(), 1..12),
    ) {
        let nifty = ladder(base, count, 0, 0);
        let bank = ladder(base, count, 1, 0);
        let mut tracker = Depth200AtmTracker::new(Depth200AtmConfig::default());
        let mut sockets = held();
        for s in spots {
            let minutes = [
                ChainMinute { underlying: DEPTH_200_ATM_UNDERLYINGS[0], spot: s, pairs: &nifty },
                ChainMinute { underlying: DEPTH_200_ATM_UNDERLYINGS[1], spot: s, pairs: &bank },
            ];
            let swaps = plan_swaps(&mut tracker, &minutes, &sockets, segment_for);
            let mut targeted: BTreeSet<usize> = BTreeSet::new();
            let mut taking: BTreeSet<(u64, u8)> = BTreeSet::new();
            for swap in &swaps {
                prop_assert!(
                    targeted.insert(swap.socket_index),
                    "socket {} swapped twice in one minute",
                    swap.socket_index
                );
                prop_assert!(
                    taking.insert((swap.new.security_id, swap.new.segment as u8)),
                    "two sockets told to take the same instrument"
                );
                if let Some(slot) = sockets.get_mut(swap.socket_index) {
                    *slot = swap.new;
                }
            }
        }
    }

    /// EVERY SWAP LANDS INSIDE THE AUTHORISED SOCKET SET.
    ///
    /// The four are NIFTY call, NIFTY put, BANKNIFTY call, BANKNIFTY put, in
    /// dial order. An index past the end would be dropped; an index inside
    /// but wrong subscribes a valid contract to the wrong connection, and
    /// nothing downstream can tell.
    #[test]
    fn every_swap_names_one_of_the_four_authorised_sockets(
        base in LADDER_BASE_LO..LADDER_BASE_HI,
        count in 1_usize..20,
        spots in prop::collection::vec(spot(), 1..12),
    ) {
        let nifty = ladder(base, count, 0, 0);
        let bank = ladder(base, count, 1, 0);
        let mut tracker = Depth200AtmTracker::new(Depth200AtmConfig::default());
        let sockets = held();
        for s in spots {
            let minutes = [
                ChainMinute { underlying: DEPTH_200_ATM_UNDERLYINGS[0], spot: s, pairs: &nifty },
                ChainMinute { underlying: DEPTH_200_ATM_UNDERLYINGS[1], spot: s, pairs: &bank },
            ];
            for swap in plan_swaps(&mut tracker, &minutes, &sockets, segment_for) {
                prop_assert!(swap.socket_index < DEPTH_200_ATM_SOCKETS);
            }
        }
    }

    /// AN ORDINARY MINUTE COSTS NOTHING.
    ///
    /// After the sockets have settled, an unchanged chain must produce an
    /// EMPTY plan. This is the claim that makes the edge-triggering worth
    /// having: a version that re-sent the current pair each minute would put
    /// ~1,500 needless messages on the wire across a session, and every
    /// swap counter would read as healthy steering.
    #[test]
    fn a_settled_chain_produces_no_swaps_at_all(
        base in LADDER_BASE_LO..LADDER_BASE_HI,
        count in 1_usize..20,
        s in spot(),
    ) {
        let nifty = ladder(base, count, 0, 0);
        let bank = ladder(base, count, 1, 0);
        let mut tracker = Depth200AtmTracker::new(Depth200AtmConfig::default());
        let mut sockets = held();
        let minutes = [
            ChainMinute { underlying: DEPTH_200_ATM_UNDERLYINGS[0], spot: s, pairs: &nifty },
            ChainMinute { underlying: DEPTH_200_ATM_UNDERLYINGS[1], spot: s, pairs: &bank },
        ];
        // First minute adopts.
        for swap in plan_swaps(&mut tracker, &minutes, &sockets, segment_for) {
            if let Some(slot) = sockets.get_mut(swap.socket_index) {
                *slot = swap.new;
            }
        }
        // Every minute after it must be silent.
        for _ in 0..30 {
            let swaps = plan_swaps(&mut tracker, &minutes, &sockets, segment_for);
            prop_assert!(swaps.is_empty(), "a settled chain produced {swaps:?}");
        }
    }

    /// AN UNNAMEABLE SEGMENT PRODUCES NOTHING.
    ///
    /// Subscribing depth on a guessed segment is a well-formed request that
    /// comes back as silence, which is indistinguishable from a quiet book.
    /// Refusing is the only honest answer.
    #[test]
    fn an_underlying_with_no_known_segment_is_never_swapped(
        base in LADDER_BASE_LO..LADDER_BASE_HI,
        count in 1_usize..20,
        spots in prop::collection::vec(spot(), 1..12),
    ) {
        let nifty = ladder(base, count, 0, 0);
        let mut tracker = Depth200AtmTracker::new(Depth200AtmConfig::default());
        let sockets = held();
        for s in spots {
            let minutes = [ChainMinute {
                underlying: DEPTH_200_ATM_UNDERLYINGS[0],
                spot: s,
                pairs: &nifty,
            }];
            let swaps = plan_swaps(&mut tracker, &minutes, &sockets, |_| None);
            prop_assert!(swaps.is_empty(), "swapped without a segment: {swaps:?}");
        }
    }

    /// AN UNTRACKED UNDERLYING IS NEVER SWAPPED.
    ///
    /// The four sockets belong to the two underlyings the operator named. A
    /// third arriving in the chain must not take one.
    #[test]
    fn an_untracked_underlying_never_takes_a_socket(
        base in LADDER_BASE_LO..LADDER_BASE_HI,
        count in 1_usize..20,
        name in "[A-Z]{3,10}",
        spots in prop::collection::vec(spot(), 1..12),
    ) {
        prop_assume!(!DEPTH_200_ATM_UNDERLYINGS.contains(&name.as_str()));
        let pairs = ladder(base, count, 2, 0);
        let mut tracker = Depth200AtmTracker::new(Depth200AtmConfig::default());
        let sockets = held();
        for s in spots {
            let minutes = [ChainMinute { underlying: &name, spot: s, pairs: &pairs }];
            let swaps = plan_swaps(&mut tracker, &minutes, &sockets, segment_for);
            prop_assert!(swaps.is_empty(), "{name} took a socket");
        }
    }

    /// A SHORT `held` DROPS THE SWAP RATHER THAN INVENTING AN OLD.
    ///
    /// Before all four sockets are dialed the caller has fewer than four
    /// entries. Fabricating an `old` for a socket that does not exist would
    /// send an unsubscribe for a contract nobody holds, on a connection that
    /// may not be there.
    #[test]
    fn fewer_sockets_than_four_never_produces_a_swap_for_a_missing_one(
        base in LADDER_BASE_LO..LADDER_BASE_HI,
        count in 1_usize..20,
        n in 0_usize..DEPTH_200_ATM_SOCKETS,
        spots in prop::collection::vec(spot(), 1..12),
    ) {
        let nifty = ladder(base, count, 0, 0);
        let bank = ladder(base, count, 1, 0);
        let mut tracker = Depth200AtmTracker::new(Depth200AtmConfig::default());
        let sockets: Vec<SubscribeInstrument> = held().into_iter().take(n).collect();
        for s in spots {
            let minutes = [
                ChainMinute { underlying: DEPTH_200_ATM_UNDERLYINGS[0], spot: s, pairs: &nifty },
                ChainMinute { underlying: DEPTH_200_ATM_UNDERLYINGS[1], spot: s, pairs: &bank },
            ];
            for swap in plan_swaps(&mut tracker, &minutes, &sockets, segment_for) {
                prop_assert!(swap.socket_index < n, "swap for socket {} of {n}", swap.socket_index);
            }
        }
    }

    /// Never panics.
    #[test]
    fn it_never_panics(
        base in LADDER_BASE_LO..LADDER_BASE_HI,
        count in 0_usize..25,
        s in prop_oneof![Just(f64::NAN), Just(0.0_f64), spot()],
        name in "[A-Za-z]{1,12}",
    ) {
        let pairs = ladder(base, count, 0, 0);
        let mut tracker = Depth200AtmTracker::new(Depth200AtmConfig::default());
        let minutes = [ChainMinute { underlying: &name, spot: s, pairs: &pairs }];
        let _ = plan_swaps(&mut tracker, &minutes, &held(), segment_for);
    }
}
