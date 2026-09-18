//! Properties of the REAL apply path — the one that mutates believed-held.
//!
//! # The gap this closes
//!
//! `depth20_track_properties` asserts that re-planning converges, but it
//! applies the swaps with a LOCAL reimplementation of the apply loop. If the
//! real `apply_depth20_plan` ever differed from that copy, the convergence
//! property would be proving something about the test rather than about
//! production. These drive the real function.
//!
//! # Why believed-held is the thing to guard
//!
//! It is the ONLY memory this subsystem has. Every future minute's diff is
//! computed against it, so an error here does not cause one bad minute — it
//! causes every remaining minute of the session to be planned against a
//! fiction. The subscribe guard then refuses swaps naming instruments the
//! connection never received, and the socket freezes while the swap counters
//! keep reporting activity.

use proptest::prelude::*;
use std::collections::BTreeSet;

use tickvault_app::depth20_layout::{Depth20Layout, Depth20Socket};
use tickvault_app::depth20_track::{Depth20LiveSocket, apply_depth20_plan, plan_depth20_minute};
use tickvault_common::types::ExchangeSegment;
use tickvault_core::websocket::pool_supervisor::{LiveSubscriptionCommand, SubscribeInstrument};

fn key(i: SubscribeInstrument) -> (u64, u8) {
    (i.security_id, i.segment as u8)
}

fn instrument() -> impl Strategy<Value = SubscribeInstrument> {
    (1_u64..8, any::<bool>()).prop_map(|(security_id, fno)| SubscribeInstrument {
        security_id,
        segment: if fno {
            ExchangeSegment::NseFno
        } else {
            ExchangeSegment::BseFno
        },
    })
}

/// Distinct, because the dial deduplicates before subscribing
/// (`dedup_subscribe_set`), so a connection cannot hold a repeat.
fn distinct_socket() -> impl Strategy<Value = Vec<SubscribeInstrument>> {
    prop::collection::vec(instrument(), 0..7).prop_map(|mut v| {
        let mut seen = BTreeSet::new();
        v.retain(|i| seen.insert(key(*i)));
        v
    })
}

fn wire() -> impl Strategy<Value = Vec<Vec<SubscribeInstrument>>> {
    prop::collection::vec(distinct_socket(), 0..5)
}

/// GLOBALLY distinct — no instrument appears twice in the whole layout,
/// neither twice on one socket nor once on each of two.
///
/// This is not a convenience: both production builders guarantee it, each
/// with ONE running `seen` set feeding a flat list that is then `chunks()`ed
/// into sockets, so a key cannot be re-issued —
/// `depth20_layout.rs::build_depth20_layout` (the pre-ranking fallback) and
/// `depth20_name_board.rs::build_name_layout` (the primary engine). Each
/// carries its own invariant test —
/// `depth20_layout_properties.rs::no_instrument_appears_twice_in_the_whole_layout`
/// and `depth20_name_board.rs::build_name_layout_never_subscribes_one_instrument_twice`
/// — and THOSE are what hold the invariant. This generator merely declines to
/// fabricate an input the builders cannot produce.
///
/// ⚠ Per-socket dedup — what `wire()` does — is NOT enough, and that is
/// MEASURED rather than assumed: the globally-distinct generator survives
/// 100,000 cases and reaches a 0-swap fixpoint within 2 minutes in 20,000
/// cases, while the per-socket-distinct-only shape FAILS inside 50,000 and a
/// duplicated `want` never settles in 3.675% of cases. The trigger is
/// specifically a CROSS-socket repeat, so this constraint is minimal.
///
/// ⚠ The mechanism is a PAIRING FLIP, not the "deferred work re-asked" shape
/// recorded in `no-rest-except-live-feed-2026-06-27.md` §12.14 — that shape
/// was tested directly and converges (1 then 1). Acquiring a duplicated key
/// changes a socket's overlap profile, so `match_sockets_by_overlap` pairs it
/// with a DIFFERENT layout socket the next minute, and the re-pairing costs
/// more swaps than the first plan. Corrected on the record in that file's
/// dated "RESOLVED 2026-09-18" block, which also carries the measurements
/// above and the reachability proof for both builders.
fn layout() -> impl Strategy<Value = Depth20Layout> {
    prop::collection::vec(prop::collection::vec(instrument(), 0..7), 0..5).prop_map(|sockets| {
        let mut seen = BTreeSet::new();
        Depth20Layout {
            sockets: sockets
                .into_iter()
                .map(|instruments| Depth20Socket {
                    underlying: None,
                    instruments: instruments
                        .into_iter()
                        .filter(|i| seen.insert(key(*i)))
                        .collect(),
                })
                .collect(),
            ..Depth20Layout::default()
        }
    })
}

/// Live sockets with roomy channels, so nothing is dropped for lack of space.
fn live(
    held: &[Vec<SubscribeInstrument>],
) -> (
    Vec<Depth20LiveSocket>,
    Vec<tokio::sync::mpsc::Receiver<LiveSubscriptionCommand>>,
) {
    let mut sockets = Vec::new();
    let mut rxs = Vec::new();
    for h in held {
        let (tx, rx) = tokio::sync::mpsc::channel(256);
        sockets.push(Depth20LiveSocket {
            tx,
            held: h.clone(),
            pending: Vec::new(),
        });
        rxs.push(rx);
    }
    (sockets, rxs)
}

proptest! {
    /// Believed-held must move EXACTLY as the wire did: every take present,
    /// every release gone.
    #[test]
    fn held_ends_where_the_sent_commands_put_it(held in wire(), want in layout()) {
        let plan = plan_depth20_minute(&held, &want);
        let (mut sockets, _rxs) = live(&held);
        apply_depth20_plan(&mut sockets, &plan);
        for socket_plan in &plan.sockets {
            let now: BTreeSet<(u64, u8)> =
                sockets[socket_plan.socket].held.iter().copied().map(key).collect();
            for (release, take) in &socket_plan.swaps {
                prop_assert!(now.contains(&key(*take)), "take {take:?} is not in held");
                prop_assert!(!now.contains(&key(*release)), "release {release:?} is still held");
            }
        }
    }

    /// A socket never changes size. Paired swaps are one out and one in, and
    /// this is the property that keeps a connection inside its 50-instrument
    /// capacity without this module tracking the limit.
    #[test]
    fn a_socket_is_the_same_size_afterwards(held in wire(), want in layout()) {
        let plan = plan_depth20_minute(&held, &want);
        let (mut sockets, _rxs) = live(&held);
        apply_depth20_plan(&mut sockets, &plan);
        for (i, socket) in sockets.iter().enumerate() {
            prop_assert_eq!(
                socket.held.len(),
                held[i].len(),
                "socket {} changed size", i
            );
        }
    }

    /// Believed-held never gains a duplicate. A repeat there would make the
    /// socket's own distinct count ambiguous, and the next minute's shape
    /// check reads that count.
    #[test]
    fn held_never_gains_a_duplicate(held in wire(), want in layout()) {
        let plan = plan_depth20_minute(&held, &want);
        let (mut sockets, _rxs) = live(&held);
        apply_depth20_plan(&mut sockets, &plan);
        for socket in &sockets {
            let distinct: BTreeSet<(u64, u8)> = socket.held.iter().copied().map(key).collect();
            prop_assert_eq!(
                distinct.len(),
                socket.held.len(),
                "believed-held gained a repeat: {:?}", socket.held
            );
        }
    }

    /// THE CONVERGENCE PROPERTY, driven through the REAL apply. A planner
    /// that never settles burns the connection's whole budget on churn while
    /// reporting healthy swap counts.
    #[test]
    fn re_planning_after_the_real_apply_is_quiet(held in wire(), want in layout()) {
        let first = plan_depth20_minute(&held, &want);
        let (mut sockets, _rxs) = live(&held);
        let sent = apply_depth20_plan(&mut sockets, &first);
        prop_assert_eq!(sent, first.swap_count(), "a roomy channel dropped a command");

        let after: Vec<Vec<SubscribeInstrument>> =
            sockets.iter().map(|s| s.held.clone()).collect();
        let second = plan_depth20_minute(&after, &want);
        prop_assert!(
            second.swap_count() <= first.swap_count(),
            "re-planning produced MORE work: {} then {}",
            first.swap_count(),
            second.swap_count()
        );
    }

    /// The commands actually put on the channel must match the plan exactly —
    /// no extras, no reordering into something the guard would refuse.
    #[test]
    fn the_channel_carries_exactly_the_planned_swaps(held in wire(), want in layout()) {
        let plan = plan_depth20_minute(&held, &want);
        let (mut sockets, mut rxs) = live(&held);
        apply_depth20_plan(&mut sockets, &plan);
        for (i, rx) in rxs.iter_mut().enumerate() {
            let mut got = Vec::new();
            while let Ok(cmd) = rx.try_recv() {
                match cmd {
                    LiveSubscriptionCommand::Swap { old, new, .. } => got.push((key(old), key(new))),
                    other => prop_assert!(false, "unexpected command {other:?}"),
                }
            }
            let expected: Vec<((u64, u8), (u64, u8))> = plan
                .sockets
                .iter()
                .find(|s| s.socket == i)
                .map(|s| s.swaps.iter().map(|(r, t)| (key(*r), key(*t))).collect())
                .unwrap_or_default();
            prop_assert_eq!(got, expected, "socket {} sent something other than its plan", i);
        }
    }

    /// A CLOSED channel must leave believed-held exactly where it was. If it
    /// moved, every future swap on that socket would name an instrument the
    /// connection never received, and the guard would refuse all of them —
    /// one dead channel costing the socket its whole day.
    #[test]
    fn a_closed_channel_never_moves_believed_held(held in wire(), want in layout()) {
        let plan = plan_depth20_minute(&held, &want);
        let mut sockets: Vec<Depth20LiveSocket> = held
            .iter()
            .map(|h| {
                let (tx, rx) = tokio::sync::mpsc::channel(16);
                drop(rx);
                Depth20LiveSocket {
                    tx,
                    held: h.clone(),
                    pending: Vec::new(),
                }
            })
            .collect();
        let sent = apply_depth20_plan(&mut sockets, &plan);
        prop_assert_eq!(sent, 0, "a closed channel accepted a command");
        for (i, socket) in sockets.iter().enumerate() {
            prop_assert_eq!(&socket.held, &held[i], "held moved on a send that never happened");
        }
    }

    /// And a closed channel must be RECOVERABLE in principle: re-planning the
    /// next minute produces the same work rather than compounding it.
    #[test]
    fn a_closed_channel_leaves_the_next_minute_planning_the_same_work(
        held in wire(),
        want in layout(),
    ) {
        let first = plan_depth20_minute(&held, &want);
        let mut sockets: Vec<Depth20LiveSocket> = held
            .iter()
            .map(|h| {
                let (tx, rx) = tokio::sync::mpsc::channel(16);
                drop(rx);
                Depth20LiveSocket {
                    tx,
                    held: h.clone(),
                    pending: Vec::new(),
                }
            })
            .collect();
        apply_depth20_plan(&mut sockets, &first);
        let after: Vec<Vec<SubscribeInstrument>> =
            sockets.iter().map(|s| s.held.clone()).collect();
        let second = plan_depth20_minute(&after, &want);
        prop_assert_eq!(second.swap_count(), first.swap_count());
    }

    /// Never panics, whatever the shapes. Index arithmetic over two
    /// independently-sized collections is where this subsystem's defects
    /// have lived.
    #[test]
    fn apply_never_panics(held in wire(), want in layout()) {
        let plan = plan_depth20_minute(&held, &want);
        let (mut sockets, _rxs) = live(&held);
        let _ = apply_depth20_plan(&mut sockets, &plan);
    }
}

// ---------------------------------------------------------------------------
// The duplicated-`want` case that `layout()` no longer generates.
//
// Constraining that generator to globally-distinct removed the ONLY coverage
// of `claimed_takes`' behaviour under a cross-socket repeat. That path is
// deliberate defence-in-depth against an input the two builders make
// unreachable, so it must stay exercised even though production cannot reach
// it — an untested defence is a defence nobody can rely on.
//
// What is asserted is exactly what `claimed_takes` guarantees and no more:
// the plan never TAKES one instrument onto two sockets. Two stronger-sounding
// properties are deliberately NOT asserted, because neither holds:
//
//   • Convergence. A duplicated `want` genuinely does not converge — that is
//     the pairing flip documented at `layout()`, and asserting it here would
//     just re-create the flake this change removes.
//
//   • "No instrument is ever HELD on two sockets." Measured while writing
//     this test: it is false, and it is false for a globally-distinct `want`
//     too. If socket A is told to take X while socket B still holds X and
//     B's departure of X goes unfunded (B has no arrival to pair it with),
//     X stays on B and also lands on A. The planner pairs departures with
//     arrivals PER SOCKET and has no cross-socket departure awareness, so a
//     transient double-hold is a property of the design, not a defect this
//     test can assert away. It resolves on a later minute once B's departure
//     is funded.
// ---------------------------------------------------------------------------

/// A cross-socket duplicate in `want` must be TAKEN once, not once per socket.
/// This is the exact shape proptest reduced to before `layout()` was
/// constrained, and it is the only remaining exercise of `claimed_takes`.
#[test]
fn a_cross_socket_duplicate_is_taken_onto_one_socket_only() {
    let dup = SubscribeInstrument {
        security_id: 7,
        segment: ExchangeSegment::BseFno,
    };
    let other = SubscribeInstrument {
        security_id: 4,
        segment: ExchangeSegment::NseFno,
    };
    // Both sockets hold something they do not want, so each can FUND an
    // arrival. Without `claimed_takes` both would therefore take `dup`.
    let held: Vec<Vec<SubscribeInstrument>> = vec![
        vec![
            SubscribeInstrument {
                security_id: 1,
                segment: ExchangeSegment::BseFno,
            },
            SubscribeInstrument {
                security_id: 2,
                segment: ExchangeSegment::BseFno,
            },
        ],
        vec![SubscribeInstrument {
            security_id: 3,
            segment: ExchangeSegment::BseFno,
        }],
    ];

    // `dup` on BOTH sockets — the shape the builders cannot emit.
    let want = Depth20Layout {
        sockets: vec![
            Depth20Socket {
                underlying: None,
                instruments: vec![dup, other],
            },
            Depth20Socket {
                underlying: None,
                instruments: vec![dup],
            },
        ],
        ..Depth20Layout::default()
    };

    let plan = plan_depth20_minute(&held, &want);

    // The guarantee: across the WHOLE plan, no key is taken twice.
    let mut taken = BTreeSet::new();
    for socket in &plan.sockets {
        for (_, take) in &socket.swaps {
            assert!(
                taken.insert(key(*take)),
                "instrument {:?} was taken onto two sockets in one plan",
                key(*take)
            );
        }
    }
    assert!(
        taken.contains(&key(dup)),
        "the duplicate was never taken at all — this case no longer exercises \
         `claimed_takes`, so the guard it protects is untested"
    );

    // And the apply path carries exactly the plan it was given.
    let (mut sockets, _rxs) = live(&held);
    let sent = apply_depth20_plan(&mut sockets, &plan);
    assert_eq!(sent, plan.swap_count(), "a roomy channel dropped a command");
}
