//! Properties of the allocator that turns instruments into the sixteen
//! sockets.
//!
//! # Why this surface
//!
//! `build_feed_stack_plan` is the last thing between a decided instrument set
//! and a live connection. Everything upstream — the layout, the ranking, the
//! expiry rule — can be perfect and still deliver nothing if this function
//! loses an instrument, opens an empty socket, or hands the same instrument to
//! two connections. It is also where the two failures this subsystem has
//! actually suffered would land: three sockets that were never dialed, and a
//! duplicate subscribe that Dhan answers with 804 — Fatal, and the connection
//! is gone for the session.
//!
//! # The honest limit
//!
//! A passing run means no counterexample was found among the cases tried. The
//! input space is infinite; these widen the search rather than closing it.

use proptest::prelude::*;
use std::collections::BTreeSet;

use tickvault_app::dhan_feed_stack::{
    FeedStackPlanError, build_feed_stack_plan, dedup_subscribe_set, distinct_fold_slots,
};
use tickvault_common::types::ExchangeSegment;
use tickvault_core::websocket::pool_budget::DhanEndpointType;
use tickvault_core::websocket::pool_supervisor::{PoolSupervisor, SubscribeInstrument};

/// The four segments the depth and main-feed sets legitimately mix.
const SEGMENTS: [ExchangeSegment; 4] = [
    ExchangeSegment::IdxI,
    ExchangeSegment::NseEquity,
    ExchangeSegment::NseFno,
    ExchangeSegment::BseFno,
];

fn key(i: &SubscribeInstrument) -> (u64, u8) {
    (i.security_id, i.segment as u8)
}

/// Instruments drawn from a deliberately SMALL id space, so repeats and
/// cross-segment collisions are common rather than astronomically rare —
/// `13` being NIFTY on `IDX_I` and something else entirely on `NSE_EQ` is the
/// I-P1-11 case, and a generator that never produces it never tests it.
fn instrument() -> impl Strategy<Value = SubscribeInstrument> {
    (0_u64..40, 0_usize..SEGMENTS.len()).prop_map(|(id, s)| SubscribeInstrument {
        security_id: id,
        segment: SEGMENTS[s],
    })
}

fn set(max: usize) -> impl Strategy<Value = Vec<SubscribeInstrument>> {
    prop::collection::vec(instrument(), 0..max)
}

/// A WIDE id space, for the capacity boundaries only.
///
/// The narrow generator above is right for collision behaviour and wrong for
/// capacity: with ids drawn from 0..40 a set of three hundred entries
/// de-duplicates to at most a hundred and sixty, so it can never reach
/// depth-20's ceiling of 250 and the refusal path is never taken. A test that
/// cannot reach the boundary it is named for proves nothing about it.
fn wide_instrument() -> impl Strategy<Value = SubscribeInstrument> {
    (0_u64..600, 0_usize..SEGMENTS.len()).prop_map(|(id, s)| SubscribeInstrument {
        security_id: id,
        segment: SEGMENTS[s],
    })
}

fn wide_set(max: usize) -> impl Strategy<Value = Vec<SubscribeInstrument>> {
    prop::collection::vec(wide_instrument(), 0..max)
}

/// Every instrument the plan actually carries, in admission order, with the
/// endpoint that carries it.
fn planned(
    main: &[SubscribeInstrument],
    d20: &[SubscribeInstrument],
    d200: &[SubscribeInstrument],
) -> Option<Vec<(DhanEndpointType, Vec<SubscribeInstrument>)>> {
    let mut pool = PoolSupervisor::new();
    let now = std::time::Instant::now();
    let plan = build_feed_stack_plan(&mut pool, now, main, d20, d200).ok()?;
    Some(
        plan.connections
            .iter()
            .map(|c| {
                (
                    c.slot.endpoint,
                    c.guard.batches().flatten().copied().collect::<Vec<_>>(),
                )
            })
            .collect(),
    )
}

proptest! {
    /// THE 804 PROPERTY. No shard may exceed the endpoint's per-connection
    /// cap.
    ///
    /// Dhan does not truncate an over-limit subscribe; it answers 804, which
    /// is Fatal, and the connection is gone for the session. The allocator
    /// derives its shard width from a division that is only safe while
    /// `needed <= available`, and that relationship is exactly the kind of
    /// arithmetic a generated test is for.
    #[test]
    fn no_socket_is_ever_over_subscribed(
        main in set(60),
        d20 in set(60),
        d200 in set(12),
    ) {
        let Some(plan) = planned(&main, &d20, &d200) else { return Ok(()); };
        for (endpoint, shard) in &plan {
            let cap = endpoint.max_instruments_per_connection() as usize;
            prop_assert!(
                shard.len() <= cap,
                "{endpoint:?} shard of {} exceeds its cap of {cap}",
                shard.len()
            );
        }
    }

    /// NOTHING IS LOST. Every instrument submitted reaches a socket.
    ///
    /// A dropped instrument is the worst failure this subsystem has, because
    /// it is invisible: no error, no counter, and a book nobody is reading
    /// while every dashboard reports the socket healthy.
    #[test]
    fn every_submitted_instrument_reaches_a_socket(
        main in set(60),
        d20 in set(60),
        d200 in set(12),
    ) {
        let Some(plan) = planned(&main, &d20, &d200) else { return Ok(()); };
        for (endpoint, submitted) in [
            (DhanEndpointType::MainFeed, &main),
            (DhanEndpointType::Depth20, &d20),
            (DhanEndpointType::Depth200, &d200),
        ] {
            let want: BTreeSet<(u64, u8)> = submitted.iter().map(key).collect();
            let got: BTreeSet<(u64, u8)> = plan
                .iter()
                .filter(|(e, _)| *e == endpoint)
                .flat_map(|(_, s)| s.iter().map(key))
                .collect();
            prop_assert_eq!(want, got, "{:?} lost or invented instruments", endpoint);
        }
    }

    /// NOTHING IS DUPLICATED WITHIN AN ENDPOINT.
    ///
    /// Two sockets of the same endpoint carrying one instrument is the 804
    /// case from the other direction — and, unlike an over-long shard, it
    /// survives every length check.
    #[test]
    fn no_instrument_is_on_two_sockets_of_one_endpoint(
        main in set(60),
        d20 in set(60),
        d200 in set(12),
    ) {
        let Some(plan) = planned(&main, &d20, &d200) else { return Ok(()); };
        for endpoint in [
            DhanEndpointType::MainFeed,
            DhanEndpointType::Depth20,
            DhanEndpointType::Depth200,
        ] {
            let mut seen: BTreeSet<(u64, u8)> = BTreeSet::new();
            for (_, shard) in plan.iter().filter(|(e, _)| *e == endpoint) {
                for i in shard {
                    prop_assert!(
                        seen.insert(key(i)),
                        "{endpoint:?} put {:?} on two sockets",
                        key(i)
                    );
                }
            }
        }
    }

    /// NO SOCKET IS OPENED EMPTY.
    ///
    /// An empty subscribe is a connection that reports healthy while carrying
    /// nothing — the false-OK the scope lock bans by name.
    #[test]
    fn no_socket_is_opened_carrying_nothing(
        main in set(60),
        d20 in set(60),
        d200 in set(12),
    ) {
        let Some(plan) = planned(&main, &d20, &d200) else { return Ok(()); };
        for (endpoint, shard) in &plan {
            prop_assert!(!shard.is_empty(), "{endpoint:?} opened an empty socket");
        }
    }

    /// THE AUTHORIZED COUNT IS NEVER EXCEEDED — per endpoint, and in total.
    ///
    /// Sixteen is an operator lock, not a suggestion: five main feed, five
    /// 20-level, five 200-level, one order update.
    #[test]
    fn the_authorized_connection_count_is_never_exceeded(
        main in set(60),
        d20 in set(60),
        d200 in set(12),
    ) {
        let Some(plan) = planned(&main, &d20, &d200) else { return Ok(()); };
        for endpoint in [
            DhanEndpointType::MainFeed,
            DhanEndpointType::Depth20,
            DhanEndpointType::Depth200,
        ] {
            let used = plan.iter().filter(|(e, _)| *e == endpoint).count();
            prop_assert!(
                used <= usize::from(endpoint.max_connections()),
                "{endpoint:?} planned {used} connections"
            );
        }
        prop_assert!(plan.len() <= 16, "planned {} connections", plan.len());
    }

    /// DEPTH SPREADS. With k instruments and a one-per-connection cap,
    /// depth-200 opens exactly k sockets — never one socket holding them all,
    /// which would strand four authorized connections.
    #[test]
    fn depth_200_opens_one_socket_per_instrument(d200 in set(12)) {
        let unique = dedup_subscribe_set(&d200).0.len();
        let Some(plan) = planned(&[], &[], &d200) else {
            // Refused because the set does not fit five connections. That is
            // the fail-closed path, tested elsewhere.
            prop_assert!(unique > 5);
            return Ok(());
        };
        let used = plan
            .iter()
            .filter(|(e, _)| *e == DhanEndpointType::Depth200)
            .count();
        prop_assert_eq!(used, unique);
    }

    /// ORDER IS PRESERVED. Concatenating an endpoint's shards gives back the
    /// de-duplicated input, in order.
    ///
    /// The per-minute tracker matches sockets to their previous contents by
    /// POSITION before falling back to content overlap, so a reordering here
    /// reads downstream as every socket having changed at once.
    #[test]
    fn shards_concatenate_back_to_the_input_in_order(
        main in set(60),
        d20 in set(60),
        d200 in set(12),
    ) {
        let Some(plan) = planned(&main, &d20, &d200) else { return Ok(()); };
        for (endpoint, submitted) in [
            (DhanEndpointType::MainFeed, &main),
            (DhanEndpointType::Depth20, &d20),
            (DhanEndpointType::Depth200, &d200),
        ] {
            let flat: Vec<(u64, u8)> = plan
                .iter()
                .filter(|(e, _)| *e == endpoint)
                .flat_map(|(_, s)| s.iter().map(key))
                .collect();
            let want: Vec<(u64, u8)> =
                dedup_subscribe_set(submitted).0.iter().map(key).collect();
            prop_assert_eq!(flat, want, "{:?} reordered its instruments", endpoint);
        }
    }

    /// FAIL-CLOSED, WHOLE-POOL. A set that does not fit refuses the plan
    /// rather than silently carrying the part that fits.
    ///
    /// Truncating would subscribe a subset and report success — the operator
    /// would see sixteen healthy sockets and a book with a hole in it.
    #[test]
    fn a_set_that_does_not_fit_refuses_rather_than_truncating(d200 in set(20)) {
        let unique = dedup_subscribe_set(&d200).0.len();
        let mut pool = PoolSupervisor::new();
        let outcome = build_feed_stack_plan(&mut pool, std::time::Instant::now(), &[], &[], &d200);
        if unique > usize::from(DhanEndpointType::Depth200.max_connections()) {
            prop_assert!(outcome.is_err(), "{unique} depth-200 instruments were accepted");
        } else {
            prop_assert!(outcome.is_ok());
        }
    }

    /// The de-duplication keys on the COMPOSITE, never the id alone (I-P1-11).
    ///
    /// Dhan reuses one numeric id across segments, so keying on the id would
    /// silently unsubscribe a real instrument — strictly worse than the
    /// duplicate it set out to remove.
    #[test]
    fn dedup_keeps_one_row_per_composite_key_and_counts_the_rest(s in set(60)) {
        let (unique, dropped) = dedup_subscribe_set(&s);
        let distinct: BTreeSet<(u64, u8)> = s.iter().map(key).collect();
        prop_assert_eq!(unique.len(), distinct.len());
        prop_assert_eq!(dropped, s.len() - distinct.len());
        let got: BTreeSet<(u64, u8)> = unique.iter().map(key).collect();
        prop_assert_eq!(got, distinct);
    }

    /// The fold sizing counts DISTINCT instruments across all three pools,
    /// not the sum of three lengths.
    ///
    /// An instrument on the main feed AND on depth-20 is one slot, not two.
    /// Counting it twice inflates the sizing toward the 25,000 ceiling for
    /// capacity the process was never going to use — and the allocator
    /// refuses a whole endpoint on that number.
    #[test]
    fn fold_slots_counts_the_union_not_the_sum(
        main in set(40),
        d20 in set(40),
        d200 in set(12),
    ) {
        let union: BTreeSet<(u64, u8)> = main
            .iter()
            .chain(d20.iter())
            .chain(d200.iter())
            .map(key)
            .collect();
        prop_assert_eq!(distinct_fold_slots(&main, &d20, &d200), union.len());
    }

    /// THE DEPTH-20 CEILING, at the boundary rather than near it.
    ///
    /// Five connections at fifty instruments each is 250 — the operator's
    /// number. At 250 the plan must open five full sockets; at 251 it must
    /// refuse the whole pool rather than carry the part that fits.
    ///
    /// This needs the WIDE id generator: the narrow one cannot produce 251
    /// distinct instruments at all, so the property would have passed
    /// vacuously and reported that a boundary it never reached was safe.
    #[test]
    fn depth_20_fills_to_its_ceiling_and_refuses_past_it(d20 in wide_set(340)) {
        let unique = dedup_subscribe_set(&d20).0.len();
        let ceiling = usize::from(DhanEndpointType::Depth20.max_connections())
            * DhanEndpointType::Depth20.max_instruments_per_connection() as usize;
        let mut pool = PoolSupervisor::new();
        let outcome = build_feed_stack_plan(&mut pool, std::time::Instant::now(), &[], &d20, &[]);
        if unique > ceiling {
            // The SPECIFIC refusal, not merely "some error".
            //
            // Past the ceiling the shard width also exceeds Dhan's
            // per-connection cap, so `SubscribeGuard` would refuse too and a
            // bare `is_err()` cannot tell the two apart. They are not
            // interchangeable: the planner's refusal is the one that names the
            // endpoint, the count and the authorised bound, and that names the
            // scope-lock file the operator has to edit. A run that fell through
            // to the guard's refusal would be correct and unreadable.
            prop_assert!(
                matches!(outcome, Err(FeedStackPlanError::PoolTooSmall { .. })),
                "{unique} past {ceiling} refused by the wrong layer: {outcome:?}"
            );
            return Ok(());
        }
        let plan = outcome.map_err(|e| TestCaseError::fail(format!("{e:?}")))?;
        let carried: usize = plan.connections.iter().map(|c| c.guard.len()).sum();
        prop_assert_eq!(carried, unique);
        for c in &plan.connections {
            prop_assert!(
                c.guard.len()
                    <= DhanEndpointType::Depth20.max_instruments_per_connection() as usize
            );
        }
    }

    /// Never panics. Three independently-sized inputs, a shared budget, and
    /// integer division are exactly where this subsystem's defects have
    /// lived.
    #[test]
    fn it_never_panics(main in wide_set(400), d20 in wide_set(340), d200 in wide_set(20)) {
        let mut pool = PoolSupervisor::new();
        let _ = build_feed_stack_plan(&mut pool, std::time::Instant::now(), &main, &d20, &d200);
    }
}
