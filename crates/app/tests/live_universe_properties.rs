//! Properties of the selector that decides the main feed's spot universe.
//!
//! # Why this surface
//!
//! `select_live_universe` decides what the main feed subscribes before any
//! contract is resolved — the ~870 NSE indices and Nifty Total Market
//! constituents. Two of its behaviours have session-ending failure modes and
//! neither can be seen from a single reading:
//!
//! - a duplicate `(security_id, segment)` in the output is a duplicate
//!   subscribe, and Dhan answers that with 804, which is Fatal: the
//!   connection is gone for the session;
//! - an over-count against the capacity envelope falls the WHOLE universe
//!   back to four index ids — a 99.98% loss of market data that, until
//!   2026-08-22, no alarm could see.
//!
//! # The honest limit
//!
//! A passing run means no counterexample was found among the cases tried. The
//! input space is infinite; these widen the search rather than closing it.

use proptest::prelude::*;
use std::collections::BTreeSet;

use tickvault_app::dhan_live_universe::{MasterEntry, UniverseSource, select_live_universe};
use tickvault_common::types::ExchangeSegment;
use tickvault_core::websocket::pool_supervisor::SubscribeInstrument;

/// The four hardcoded index seeds, in the shape the caller passes them.
fn seeds() -> Vec<SubscribeInstrument> {
    [13_u64, 25, 51, 21]
        .into_iter()
        .map(|security_id| SubscribeInstrument {
            security_id,
            segment: ExchangeSegment::IdxI,
        })
        .collect()
}

fn key(i: &SubscribeInstrument) -> (u64, u8) {
    (i.security_id, i.segment as u8)
}

/// Master entries over a deliberately SMALL id space and a segment byte that
/// includes Dhan's documented GAP at 6.
///
/// Small ids make repeats common — a repeat is the whole point here. The gap
/// byte makes the refuse-rather-than-coerce path a normal case: coercing an
/// unknown segment would subscribe a real id under the wrong one, and
/// `(security_id, segment)` is the identity everything downstream keys on.
fn entry() -> impl Strategy<Value = MasterEntry> {
    // 0 is a refusable id; 13/25/51/21 collide with the seeds on purpose.
    (
        prop_oneof![Just(0_u64), Just(13), Just(25), 1_u64..40],
        prop_oneof![
            Just(0_u8),
            Just(1),
            Just(2),
            Just(4),
            Just(6),
            Just(8),
            Just(99)
        ],
    )
        .prop_map(|(security_id, exchange_segment_code)| MasterEntry {
            security_id,
            exchange_segment_code,
        })
}

fn master() -> impl Strategy<Value = Vec<MasterEntry>> {
    prop::collection::vec(entry(), 0..40)
}

proptest! {
    /// THE 804 PROPERTY. No instrument may appear twice in the selection.
    ///
    /// A duplicate subscribe is answered with 804 — Fatal — and the
    /// connection is gone for the rest of the session. Downstream dedup
    /// exists, but a selection that relies on someone else to remove its
    /// duplicates has also mis-counted itself against the capacity envelope,
    /// and that over-count falls the whole universe back to four ids.
    #[test]
    fn no_instrument_is_selected_twice(m in master(), capacity in 0_usize..80) {
        let got = select_live_universe(&seeds(), Some(&m), capacity);
        let mut seen: BTreeSet<(u64, u8)> = BTreeSet::new();
        for inst in &got.instruments {
            prop_assert!(seen.insert(key(inst)), "{:?} selected twice", key(inst));
        }
    }

    /// THE ENVELOPE IS NEVER EXCEEDED.
    ///
    /// Over the envelope, `plan_pool` refuses the WHOLE main-feed pool, so an
    /// overshoot costs the session its price feed rather than costing the
    /// excess. This selector's own answer to that is to fall back, and the
    /// fallback must itself fit.
    #[test]
    fn the_selection_never_exceeds_the_capacity(m in master(), capacity in 0_usize..80) {
        let got = select_live_universe(&seeds(), Some(&m), capacity);
        if got.source == UniverseSource::MasterSourced {
            prop_assert!(got.instruments.len() <= capacity);
        }
    }

    /// NOTHING IS INVENTED. Every instrument is a seed or a master entry.
    #[test]
    fn every_instrument_came_from_a_seed_or_the_master(m in master(), capacity in 0_usize..80) {
        let seeds = seeds();
        let allowed: BTreeSet<(u64, u8)> = seeds
            .iter()
            .map(key)
            .chain(m.iter().filter_map(|e| {
                ExchangeSegment::from_byte(e.exchange_segment_code)
                    .map(|s| (e.security_id, s as u8))
            }))
            .collect();
        let got = select_live_universe(&seeds, Some(&m), capacity);
        for inst in &got.instruments {
            prop_assert!(allowed.contains(&key(inst)), "invented {inst:?}");
        }
    }

    /// A MASTER THAT SUPPLIES INDICES REPLACES THE SEEDS.
    ///
    /// The four seeds are REST-API ids reused on the WebSocket, and they were
    /// measured receiving zero packets of any code — including the code-6
    /// PrevClose Dhan support confirmed is emitted for IDX_I on any
    /// subscription. Keeping both would subscribe an id we have evidence is
    /// dead beside the one that should work, and report the pair as coverage.
    #[test]
    fn master_indices_replace_the_seeds_rather_than_joining_them(
        m in master(),
        capacity in 0_usize..80,
    ) {
        let seeds = seeds();
        let master_index_ids: BTreeSet<u64> = m
            .iter()
            .filter(|e| {
                e.security_id != 0
                    && ExchangeSegment::from_byte(e.exchange_segment_code)
                        == Some(ExchangeSegment::IdxI)
            })
            .map(|e| e.security_id)
            .collect();
        if master_index_ids.is_empty() {
            return Ok(());
        }
        let got = select_live_universe(&seeds, Some(&m), capacity);
        if got.source != UniverseSource::MasterSourced {
            return Ok(());
        }
        for inst in got.instruments.iter().filter(|i| i.segment == ExchangeSegment::IdxI) {
            prop_assert!(
                master_index_ids.contains(&inst.security_id),
                "seed {} survived a master that supplied indices",
                inst.security_id
            );
        }
    }

    /// A MASTER WITH NO INDICES LEAVES THE SEEDS IN PLACE.
    ///
    /// A broken master must degrade to the old behaviour, not to no indices at
    /// all — that would turn a data problem into an outage.
    #[test]
    fn a_master_with_no_indices_keeps_the_seeds(m in master(), capacity in 20_usize..80) {
        let seeds = seeds();
        let has_index = m.iter().any(|e| {
            e.security_id != 0
                && ExchangeSegment::from_byte(e.exchange_segment_code)
                    == Some(ExchangeSegment::IdxI)
        });
        if has_index {
            return Ok(());
        }
        let got = select_live_universe(&seeds, Some(&m), capacity);
        let kept: BTreeSet<(u64, u8)> = got.instruments.iter().map(key).collect();
        for seed in &seeds {
            prop_assert!(kept.contains(&key(seed)), "seed {seed:?} was dropped");
        }
    }

    /// OVER CAPACITY FALLS BACK TO EXACTLY THE SEEDS, AND SAYS SO.
    ///
    /// A silent partial widening would be worse than the fallback: the count
    /// would look ordinary and nobody could tell which instruments were lost.
    #[test]
    fn an_oversized_master_falls_back_to_the_index_set_and_reports_it(m in master()) {
        let seeds = seeds();
        let got = select_live_universe(&seeds, Some(&m), 0);
        prop_assert_eq!(got.source, UniverseSource::FellBackToIndices);
        prop_assert_eq!(
            got.instruments.iter().map(key).collect::<Vec<_>>(),
            seeds.iter().map(key).collect::<Vec<_>>()
        );
    }

    /// THE REFUSAL COUNTERS DESCRIBE WHAT HAPPENED.
    ///
    /// A zero id and an undecodable segment byte are different vendor
    /// problems with different remedies, and a counter that merges them sends
    /// triage to the wrong column. Dhan's segment numbering has a documented
    /// GAP at 6, so the undecodable case is real, not hypothetical.
    #[test]
    fn the_refusal_counters_match_the_input(m in master(), capacity in 0_usize..80) {
        let got = select_live_universe(&seeds(), Some(&m), capacity);
        let zero = m.iter().filter(|e| e.security_id == 0).count();
        let unknown = m
            .iter()
            .filter(|e| {
                e.security_id != 0 && ExchangeSegment::from_byte(e.exchange_segment_code).is_none()
            })
            .count();
        prop_assert_eq!(got.refused_zero_id, zero);
        prop_assert_eq!(got.refused_unknown_segment, unknown);
    }

    /// NO MASTER AT ALL IS THE HARDCODED SET, NAMED AS SUCH.
    ///
    /// Distinct from a master that resolved nothing: one is "we were not asked
    /// to widen", the other is "we were asked and could not", and they need
    /// different responses.
    #[test]
    fn an_absent_master_is_the_hardcoded_set(capacity in 0_usize..80) {
        let seeds = seeds();
        let got = select_live_universe(&seeds, None, capacity);
        prop_assert_eq!(got.source, UniverseSource::HardcodedIndices);
        prop_assert_eq!(
            got.instruments.iter().map(key).collect::<Vec<_>>(),
            seeds.iter().map(key).collect::<Vec<_>>()
        );
    }

    /// ORDER-INDEPENDENT SET. Reversing the master rows cannot change WHICH
    /// instruments are subscribed.
    ///
    /// The artifact is a file, and a file's row order is not a contract.
    #[test]
    fn reversing_the_master_does_not_change_the_subscribed_set(
        m in master(),
        capacity in 0_usize..80,
    ) {
        let seeds = seeds();
        let forward = select_live_universe(&seeds, Some(&m), capacity);
        let reversed: Vec<MasterEntry> = m.iter().rev().cloned().collect();
        let backward = select_live_universe(&seeds, Some(&reversed), capacity);
        prop_assert_eq!(
            forward.instruments.iter().map(key).collect::<BTreeSet<_>>(),
            backward.instruments.iter().map(key).collect::<BTreeSet<_>>()
        );
        prop_assert_eq!(forward.source, backward.source);
    }

    /// Never panics.
    #[test]
    fn it_never_panics(m in master(), capacity in 0_usize..80) {
        let _ = select_live_universe(&seeds(), Some(&m), capacity);
    }
}
