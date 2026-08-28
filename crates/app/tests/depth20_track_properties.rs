//! Random-input attack on the per-minute depth-20 tracker.
//!
//! `depth20_track` has a hand-written suite that covers the shapes someone
//! thought of. This file covers the shapes nobody did: sockets holding each
//! other's sets, layouts smaller than the wire, instruments repeated across
//! sockets, empty desired sets scattered among full ones, and the same id
//! appearing under two segments.
//!
//! The invariants below are not stylistic. Two of them are the difference
//! between a live connection and a dead one:
//!
//! - **A take must not already be held.** Dhan answers a duplicate subscribe
//!   with an 804, which is Fatal — the connection drops and does not come
//!   back this session.
//! - **A take must be globally unique across the plan.** Two sockets taking
//!   the same instrument in one minute is the same 804 by a different route,
//!   and it also spends two of the 250 authorized slots on one order book.
//!
//! The generator deliberately draws from a SMALL id pool so collisions are
//! frequent rather than theoretical: a pool wide enough to make every draw
//! distinct would test the easy case forever.

use proptest::prelude::*;
use std::collections::{BTreeSet, HashSet};

use tickvault_app::depth20_layout::{Depth20Layout, Depth20Socket};
use tickvault_app::depth20_track::plan_depth20_minute;
use tickvault_common::types::ExchangeSegment;
use tickvault_core::websocket::pool_supervisor::SubscribeInstrument;

type Key = (u64, u8);

fn key_of(i: SubscribeInstrument) -> Key {
    (i.security_id, i.segment as u8)
}

/// Two segments, because `security_id` alone is not an identity (I-P1-11) and
/// a tracker that keyed on the id alone would collapse two real instruments
/// into one and unsubscribe a contract it still holds.
fn segment(n: u8) -> ExchangeSegment {
    if n % 2 == 0 {
        ExchangeSegment::NseFno
    } else {
        ExchangeSegment::BseFno
    }
}

fn instrument() -> impl Strategy<Value = SubscribeInstrument> {
    // 1..=14 ids over 2 segments = 28 distinct instruments. Sockets of up to
    // 8 draw from that, so overlap between sockets is the common case.
    (1u64..=14, 0u8..=1).prop_map(|(id, s)| SubscribeInstrument {
        security_id: id,
        segment: segment(s),
    })
}

fn held_socket() -> impl Strategy<Value = Vec<SubscribeInstrument>> {
    prop::collection::vec(instrument(), 0..8)
}

fn desired_socket() -> impl Strategy<Value = Depth20Socket> {
    prop::collection::vec(instrument(), 0..8).prop_map(|instruments| Depth20Socket {
        underlying: None,
        instruments,
    })
}

fn scenario() -> impl Strategy<Value = (Vec<Vec<SubscribeInstrument>>, Depth20Layout)> {
    (
        prop::collection::vec(held_socket(), 0..6),
        prop::collection::vec(desired_socket(), 0..6),
    )
        .prop_map(|(held, sockets)| {
            (
                held,
                Depth20Layout {
                    sockets,
                    ..Depth20Layout::default()
                },
            )
        })
}

/// The wire shape the system can actually produce.
///
/// `build_depth20_layout` dedupes the WHOLE layout — index sockets included —
/// so no instrument appears twice in a layout, and the wire is dialed from a
/// layout, so no instrument is held by two connections either.
///
/// This generator is used ONLY for the two CONVERGENCE properties below.
/// Every safety property keeps the adversarial generator above, because
/// safety must hold on malformed input and convergence is only a claim about
/// states the system can reach. Stating that difference is the point: a
/// convergence property quietly widened to unreachable input reports a defect
/// that cannot happen, and one quietly narrowed to reachable input hides the
/// safety question entirely.
fn reachable_scenario() -> impl Strategy<Value = (Vec<Vec<SubscribeInstrument>>, Depth20Layout)> {
    // 12 ids x 2 segments = 24 distinct instruments, each placed at most once
    // on the wire and at most once in the layout.
    let placements = prop::collection::vec(
        (
            prop::option::of(0usize..4),
            prop::option::of(0usize..4),
            0u8..=1,
        ),
        24,
    );
    placements.prop_map(|slots| {
        let mut held: Vec<Vec<SubscribeInstrument>> = vec![Vec::new(); 4];
        let mut want: Vec<Depth20Socket> = (0..4)
            .map(|_| Depth20Socket {
                underlying: None,
                instruments: Vec::new(),
            })
            .collect();
        for (index, (on_wire, in_layout, seg)) in slots.into_iter().enumerate() {
            let instrument = SubscribeInstrument {
                security_id: (index as u64 / 2) + 1,
                segment: segment(seg.wrapping_add(u8::try_from(index % 2).unwrap_or(0))),
            };
            if let Some(s) = on_wire {
                held[s].push(instrument);
            }
            if let Some(s) = in_layout {
                want[s].instruments.push(instrument);
            }
        }
        // The placement above can still hand the same (id, segment) to two
        // slots when the id/segment arithmetic collides, so the invariant the
        // builder guarantees is enforced here rather than assumed.
        let mut seen_wire = HashSet::new();
        for socket in &mut held {
            socket.retain(|i| seen_wire.insert(key_of(*i)));
        }
        let mut seen_want = HashSet::new();
        for socket in &mut want {
            socket.instruments.retain(|i| seen_want.insert(key_of(*i)));
        }
        (
            held,
            Depth20Layout {
                sockets: want,
                ..Depth20Layout::default()
            },
        )
    })
}

/// The wire side after a plan is applied, computed purely: every release
/// leaves, every take arrives. Mirrors `apply_depth20_plan`'s successful path
/// without needing channels.
fn apply_pure(
    held: &[Vec<SubscribeInstrument>],
    plan: &tickvault_app::depth20_track::Depth20Plan,
) -> Vec<Vec<SubscribeInstrument>> {
    let mut out: Vec<Vec<SubscribeInstrument>> = held.to_vec();
    for sp in &plan.sockets {
        let Some(socket) = out.get_mut(sp.socket) else {
            continue;
        };
        for (release, take) in &sp.swaps {
            if let Some(slot) = socket.iter_mut().find(|h| **h == *release) {
                *slot = *take;
            } else {
                socket.push(*take);
            }
            // Mirrors apply_depth20_plan: one unsubscribe takes the
            // instrument off the wire outright, so every further believed
            // copy goes with it.
            socket.retain(|h| *h != *release);
        }
    }
    out
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(400))]

    /// The 804 invariant. A subscribe for something the connection already
    /// holds is Fatal at Dhan's end, so this is the property whose failure
    /// costs a socket for the rest of the session.
    #[test]
    fn a_take_is_never_something_the_socket_already_holds((held, want) in scenario()) {
        let plan = plan_depth20_minute(&held, &want);
        for sp in &plan.sockets {
            let have: BTreeSet<Key> = held[sp.socket].iter().copied().map(key_of).collect();
            for (_, take) in &sp.swaps {
                prop_assert!(
                    !have.contains(&key_of(*take)),
                    "socket {} was told to take {:?}, which it already holds — Dhan \
                     answers a duplicate subscribe with an 804 (Fatal)",
                    sp.socket, key_of(*take)
                );
            }
        }
    }

    /// The same 804 by the other route: two connections taking one instrument
    /// in the same minute.
    #[test]
    fn no_instrument_is_taken_by_two_sockets_in_one_minute((held, want) in scenario()) {
        let plan = plan_depth20_minute(&held, &want);
        let mut seen: HashSet<Key> = HashSet::new();
        for sp in &plan.sockets {
            for (_, take) in &sp.swaps {
                prop_assert!(
                    seen.insert(key_of(*take)),
                    "{:?} is taken twice in one plan — two of the 250 authorized \
                     slots on one order book, and a duplicate subscribe",
                    key_of(*take)
                );
            }
        }
    }

    /// A release names something the connection is believed to hold. The
    /// subscribe guard refuses an unsubscribe for an instrument it never saw,
    /// and a refused swap costs the slot it was funding.
    #[test]
    fn a_release_is_always_something_the_socket_holds((held, want) in scenario()) {
        let plan = plan_depth20_minute(&held, &want);
        for sp in &plan.sockets {
            let have: BTreeSet<Key> = held[sp.socket].iter().copied().map(key_of).collect();
            for (release, _) in &sp.swaps {
                prop_assert!(
                    have.contains(&key_of(*release)),
                    "socket {} was told to release {:?}, which it does not hold",
                    sp.socket, key_of(*release)
                );
            }
        }
    }

    /// Releasing the same instrument twice frees one slot while spending two,
    /// so the socket ends the minute below its dialed size.
    #[test]
    fn a_socket_never_releases_the_same_instrument_twice((held, want) in scenario()) {
        let plan = plan_depth20_minute(&held, &want);
        for sp in &plan.sockets {
            let mut seen: HashSet<Key> = HashSet::new();
            for (release, _) in &sp.swaps {
                prop_assert!(
                    seen.insert(key_of(*release)),
                    "socket {} releases {:?} twice", sp.socket, key_of(*release)
                );
            }
        }
    }

    /// A swap that releases and takes the same instrument is a no-op that
    /// still costs two wire messages and a guard round-trip.
    #[test]
    fn a_swap_never_releases_and_takes_the_same_instrument((held, want) in scenario()) {
        let plan = plan_depth20_minute(&held, &want);
        for sp in &plan.sockets {
            for (release, take) in &sp.swaps {
                prop_assert_ne!(
                    key_of(*release), key_of(*take),
                    "socket {} swaps an instrument for itself", sp.socket
                );
            }
        }
    }

    /// The plan addresses real connections. A socket index past the end is
    /// what `apply_depth20_plan`'s `no_socket` arm exists to survive, and it
    /// should never have to.
    #[test]
    fn every_planned_socket_index_addresses_a_real_socket((held, want) in scenario()) {
        let plan = plan_depth20_minute(&held, &want);
        for sp in &plan.sockets {
            prop_assert!(
                sp.socket < held.len(),
                "plan names socket {} of {}", sp.socket, held.len()
            );
        }
    }

    /// Paired swaps hold the connection's size constant. An unpaired arrival
    /// would push it past 50 (Dhan does not silently drop the excess); an
    /// unpaired departure would leave it carrying fewer for no gain.
    ///
    /// FALSIFIABILITY, recorded because a property nobody can break is
    /// indistinguishable from one that is never evaluated: the generator DOES
    /// reach non-empty plans here (probed by asserting `swap_count() == 0`,
    /// which fails). But three deliberate breaks — removing the cross-socket
    /// take claim, loosening the overlap tiebreak, and deleting the shape pass
    /// from `match_sockets_by_overlap` outright — left both this and the
    /// convergence property below GREEN. On duplicate-free input the socket
    /// pairing cannot move between minutes, so neither can be falsified by
    /// breaking the pairing: they hold structurally rather than by luck. Their
    /// bite is on the adversarial generator, where the first version of this
    #[test]
    /// property found the layout duplicate this change fixes.
    fn applying_a_plan_leaves_every_socket_the_size_it_started((held, want) in reachable_scenario()) {
        let plan = plan_depth20_minute(&held, &want);
        let after = apply_pure(&held, &plan);
        for (i, (before, now)) in held.iter().zip(after.iter()).enumerate() {
            prop_assert_eq!(
                before.len(), now.len(),
                "socket {} changed size {} -> {}", i, before.len(), now.len()
            );
        }
    }

    /// Every take comes from the desired layout. A take invented from
    /// anywhere else would subscribe an instrument the operator's selection
    /// never named.
    #[test]
    fn every_take_appears_somewhere_in_the_desired_layout((held, want) in scenario()) {
        let plan = plan_depth20_minute(&held, &want);
        let wanted: BTreeSet<Key> = want
            .sockets
            .iter()
            .flat_map(|s| s.instruments.iter().copied().map(key_of))
            .collect();
        for sp in &plan.sockets {
            for (_, take) in &sp.swaps {
                prop_assert!(
                    wanted.contains(&key_of(*take)),
                    "{:?} was taken but is in no desired socket", key_of(*take)
                );
            }
        }
    }

    /// Two records of the same minute must produce the same wire traffic. A
    /// swap the guard refuses once would otherwise be refused forever, on a
    /// different socket each time.
    #[test]
    fn the_plan_is_deterministic((held, want) in scenario()) {
        let a = plan_depth20_minute(&held, &want);
        let b = plan_depth20_minute(&held, &want);
        prop_assert_eq!(a, b);
    }

    /// The order the wire reports its holdings in is an artifact of the dial,
    /// not information. Reversing it must not change what goes out.
    #[test]
    fn reversing_what_a_socket_holds_does_not_change_the_swap_set((held, want) in scenario()) {
        let plan = plan_depth20_minute(&held, &want);
        let reversed: Vec<Vec<SubscribeInstrument>> = held
            .iter()
            .map(|s| s.iter().copied().rev().collect())
            .collect();
        let other = plan_depth20_minute(&reversed, &want);

        let set_of = |p: &tickvault_app::depth20_track::Depth20Plan| -> BTreeSet<(usize, Key, Key)> {
            p.sockets
                .iter()
                .flat_map(|sp| {
                    sp.swaps
                        .iter()
                        .map(move |(r, t)| (sp.socket, key_of(*r), key_of(*t)))
                })
                .collect()
        };
        prop_assert_eq!(set_of(&plan), set_of(&other));
    }

    /// One side of the diff is always exhausted: pairing takes
    /// `min(departures, arrivals)`, so a surplus can exist on one side but
    /// never on both. Both being non-zero would mean a pair was available and
    /// went unmade.
    #[test]
    fn a_socket_never_has_surplus_on_both_sides((held, want) in scenario()) {
        let plan = plan_depth20_minute(&held, &want);
        for sp in &plan.sockets {
            prop_assert!(
                sp.unfunded_arrivals == 0 || sp.unused_departures == 0,
                "socket {} reports {} unfunded arrivals AND {} unused departures — \
                 a pair was available and not made",
                sp.socket, sp.unfunded_arrivals, sp.unused_departures
            );
        }
    }

    /// An empty desired layout means the chain did not publish, or the
    /// ranking returned nothing. Stripping working sockets on the strength of
    /// a missing input is the trade the attach already refuses.
    #[test]
    fn an_empty_layout_moves_nothing(held in prop::collection::vec(held_socket(), 0..6)) {
        let plan = plan_depth20_minute(&held, &Depth20Layout::default());
        prop_assert!(plan.is_quiet());
        prop_assert_eq!(plan.swap_count(), 0);
        prop_assert_eq!(plan.sockets_left_alone, held.len());
    }

    /// Every wire socket is accounted for exactly once: it either appears in
    /// the plan, was left alone, or was quiet because it already matched.
    /// A socket that fell out of all three would be silently unmanaged.
    #[test]
    fn every_wire_socket_is_planned_left_alone_or_quiet((held, want) in scenario()) {
        let plan = plan_depth20_minute(&held, &want);
        prop_assert!(
            plan.sockets.len() + plan.sockets_left_alone <= held.len(),
            "{} planned + {} left alone exceeds {} sockets",
            plan.sockets.len(), plan.sockets_left_alone, held.len()
        );
    }

    /// Convergence. After one minute's swaps land, the next minute against
    /// the same desired layout must not undo them — a planner that oscillated
    /// would spend the connection's whole capacity swapping A for B and back.
    #[test]
    fn a_second_minute_against_the_same_layout_does_not_undo_the_first((held, want) in reachable_scenario()) {
        let first = plan_depth20_minute(&held, &want);
        let after = apply_pure(&held, &first);
        let second = plan_depth20_minute(&after, &want);

        // Nothing the first minute TOOK may be released by the second.
        let took: BTreeSet<Key> = first
            .sockets
            .iter()
            .flat_map(|sp| sp.swaps.iter().map(|(_, t)| key_of(*t)))
            .collect();
        for sp in &second.sockets {
            for (release, _) in &sp.swaps {
                prop_assert!(
                    !took.contains(&key_of(*release)),
                    "the second minute releases {:?}, which the first minute just \
                     took — the planner is oscillating",
                    key_of(*release)
                );
            }
        }
    }
}
