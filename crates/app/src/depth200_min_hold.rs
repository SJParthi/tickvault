//! Depth-200 minimum hold: a swapped-in contract stays at least 10 minutes.
//!
//! # Why (scope lock 2026-10-20)
//!
//! `websocket-connection-scope-lock.md`, "2026-10-20 — DEPTH-200 HOLDS A
//! SWAPPED-IN CONTRACT FOR AT LEAST 10 MINUTES". Measured from the live logs
//! (14, 15, 16 and 19 Oct): depth-200 swapped about 1,200 times a day and a
//! socket received about 2 frames per swap, so the pool carried mostly opening
//! snapshots. The owner chose "Hold 10 min (Recommended)": keep the
//! top-5-by-1-minute-volume rule, but once a contract is on a socket it stays
//! at least [`DEPTH200_MIN_HOLD_SECS`] (600 s), counted from its swap-in.
//!
//! # What this module holds
//!
//! [`Depth200HoldBook`]: per depth-200 socket, the contract the steering loop
//! swapped onto it and the MONOTONIC instant it did so, plus the one before it
//! (so a swap the connection refused, which the reconcile reverts, gets its
//! old contract's swap-in time back rather than a fresh one). Each minute the
//! loop asks it for one `Option<u64>` per socket ([`Depth200HoldBook::locked_mask`]:
//! the held contract's underlying when the socket is locked), which
//! `depth200_ranked_steer::plan_ranked_minute` reads. The underlying is what
//! keeps the 2026-09-24 "top 5 distinct underlyings" rule whole: a locked
//! socket keeps its name, so the planner never places another contract of
//! that underlying on a second socket while the hold runs.
//!
//! # What the hold does NOT cover (the rule's own carve-outs)
//!
//! * A socket that holds nothing is never held: the mask is `false` for it.
//! * A contract this loop did not swap in (placed by the boot dial or the
//!   seed, or a hold that changed behind the loop's back) has no swap-in time
//!   and is not held. The rule counts from a SWAP-IN; inventing one would hold
//!   a dial-placed contract that the first ranking is meant to replace.
//! * A contract gone from the board's eligible universe is not held
//!   ([`depth200_held_underlying`]): it must be in today's
//!   contract-to-underlying map, which is built from today's contract artifact
//!   (so an expired contract is absent), and be a STOCK option (the only family
//!   depth-200 may carry, 2026-09-23). The gainer filter is NOT part of
//!   eligibility: a gainer verdict flips minute to minute, and releasing on it
//!   would defeat the hold.
//!
//! The ranking, the hysteresis band, the per-socket and pool-wide caps,
//! `ROTATION_HALTED`, the 805 handling and the silent-swap resubscribe
//! (2026-10-13) are untouched; depth-20 and the main feed never read this.
//!
//! # Honest limits
//!
//! * The loop runs once a minute, so a contract leaves between 600 s and about
//!   660 s after its swap-in (the first minute at or past 600 s). It never
//!   leaves before 600 s.
//! * The book lives in the steering loop's memory. After a restart every
//!   contract on the wire reads as dial-placed (no swap-in time) and is not
//!   held until it is swapped again.
//! * An empty eligibility map (nothing attached yet) reads every contract as
//!   ineligible, so nothing is held: the failure direction is the behaviour
//!   before this rule, never a contract stuck on a socket.
//!
//! # Complexity
//!
//! Per minute, on the steering task (never on the frame drain, never per
//! frame): [`Depth200HoldBook::observe`] O(sockets),
//! [`Depth200HoldBook::locked_mask`] O(sockets) with one O(1)-average hash
//! probe each, [`Depth200HoldBook::note_sent_swaps`] O(swaps). Sockets is the
//! depth-200 budget (5). The mask is a fresh `Vec<Option<u64>>` of at most five
//! entries a minute, cold. The swap-in stamp is read after the send returns
//! (`depth_rebalance`), so a stamp never predates the real send.

use std::time::{Duration, Instant};

use tickvault_core::websocket::pool_supervisor::SubscribeInstrument;

use crate::contract_underlying_map::ContractUnderlyingMap;
use crate::depth200_atm::PlannedSwap;
use crate::volume_leaderboard::OptionFamily;

/// The minimum time a contract swapped onto a depth-200 socket stays there.
///
/// 600 s, the owner's 2026-10-20 choice. The scope lock REJECTS a different
/// value (above or below) without a fresh dated quote, so it is a constant,
/// not a config knob.
pub const DEPTH200_MIN_HOLD_SECS: u64 = 600;

/// One swap-in: the contract and the monotonic instant it was queued.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SwapIn {
    instrument: SubscribeInstrument,
    at: Instant,
}

/// One socket's record: the current swap-in and the one before it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct HoldSlot {
    current: Option<SwapIn>,
    /// The swap-in the current one replaced, kept so a refused swap that the
    /// reconcile reverts restores its contract's original time.
    prior: Option<SwapIn>,
}

/// Per-socket swap-in times for the depth-200 pool. Owned by the steering
/// loop; see the module header.
#[derive(Debug, Clone, Default)]
pub struct Depth200HoldBook {
    slots: Vec<HoldSlot>,
}

impl Depth200HoldBook {
    /// A book for `sockets` depth-200 sockets, none of them held.
    #[must_use]
    pub fn new(sockets: usize) -> Self {
        Self {
            slots: vec![HoldSlot::default(); sockets],
        }
    }

    /// Lines the book up with what each socket holds now (after the
    /// reconcile), once a minute, before [`Self::locked_mask`].
    ///
    /// | `held[i]` | action |
    /// |---|---|
    /// | the current swap-in's contract | none |
    /// | the prior swap-in's contract (a refused swap, reverted) | the prior becomes current |
    /// | anything else, or nothing | no swap-in time: not held |
    ///
    /// O(sockets). Resizes only if the socket count changed, which it does not
    /// within a session.
    pub fn observe(&mut self, held: &[Option<SubscribeInstrument>]) {
        if self.slots.len() != held.len() {
            self.slots.resize(held.len(), HoldSlot::default());
        }
        for (slot, now_held) in self.slots.iter_mut().zip(held) {
            let current = slot.current.map(|s| s.instrument);
            if now_held.is_some() && current == *now_held {
                continue;
            }
            let prior = slot.prior.map(|s| s.instrument);
            *slot = if now_held.is_some() && prior == *now_held {
                HoldSlot {
                    current: slot.prior,
                    prior: None,
                }
            } else {
                HoldSlot::default()
            };
        }
    }

    /// Records the swaps this minute actually queued, stamped `at`.
    ///
    /// A swap counts as sent only when its socket now holds `swap.new`
    /// (`held_of(i)`): the send advances the believed hold only on a successful
    /// queue, so a swap refused at the channel (full, closed, or an ack still
    /// pending) leaves no stamp. Returns how many swaps were stamped.
    /// O(swaps).
    pub fn note_sent_swaps(
        &mut self,
        swaps: &[PlannedSwap],
        held_of: impl Fn(usize) -> Option<SubscribeInstrument>,
        at: Instant,
    ) -> usize {
        let mut stamped = 0usize;
        for swap in swaps {
            if held_of(swap.socket_index) != Some(swap.new) {
                continue;
            }
            if self.slots.len() <= swap.socket_index {
                self.slots
                    .resize(swap.socket_index.saturating_add(1), HoldSlot::default());
            }
            let Some(slot) = self.slots.get_mut(swap.socket_index) else {
                continue;
            };
            // The departing contract's own swap-in, if the book had one for
            // it, so a revert of THIS swap gives it back.
            let prior = slot.current.filter(|s| s.instrument == swap.old);
            *slot = HoldSlot {
                current: Some(SwapIn {
                    instrument: swap.new,
                    at,
                }),
                prior,
            };
            stamped = stamped.saturating_add(1);
        }
        stamped
    }

    /// One `Option<u64>` per socket: `Some(underlying)` when the socket holds
    /// the contract this book stamped, less than [`DEPTH200_MIN_HOLD_SECS`]
    /// before `now`, and `underlying_of` still accepts that contract (returns
    /// its underlying); `None` otherwise.
    ///
    /// The underlying is returned, not a bare `bool`, so the planner can keep
    /// the locked socket's NAME: an entry candidate of the same underlying is
    /// not placed on another socket while the hold runs (two sockets on one
    /// underlying would break the 2026-09-24 "top 5 distinct underlyings"
    /// rule).
    ///
    /// `held` must be the same snapshot passed to [`Self::observe`] this
    /// minute. A socket that holds nothing, an unstamped contract and an
    /// ineligible one all read `None`. Monotonic time, so a wall-clock step
    /// can neither shorten nor lengthen a hold. O(sockets) plus one
    /// `underlying_of` call per stamped, unexpired socket.
    #[must_use]
    pub fn locked_mask(
        &self,
        held: &[Option<SubscribeInstrument>],
        now: Instant,
        underlying_of: impl Fn(&SubscribeInstrument) -> Option<u64>,
    ) -> Vec<Option<u64>> {
        let hold = Duration::from_secs(DEPTH200_MIN_HOLD_SECS);
        held.iter()
            .enumerate()
            .map(|(index, now_held)| {
                let instrument = now_held.as_ref()?;
                let stamp = self.slots.get(index).and_then(|s| s.current)?;
                if stamp.instrument != *instrument
                    || now.saturating_duration_since(stamp.at) >= hold
                {
                    return None;
                }
                underlying_of(instrument)
            })
            .collect()
    }
}

/// The underlying of a held depth-200 contract when it is still in the
/// board's eligible universe: present in today's contract-to-underlying map
/// (built from today's contract artifact, so an expired contract is absent)
/// and a STOCK option. `None` for an ineligible contract, which the hold then
/// does not keep.
///
/// O(1) average: one atomic load and one hash probe on the composite
/// `(security_id, segment)` key.
#[must_use]
pub fn depth200_held_underlying(
    map: &ContractUnderlyingMap,
    instrument: &SubscribeInstrument,
) -> Option<u64> {
    map.owner_of(instrument.security_id, instrument.segment)
        .filter(|owner| owner.family == OptionFamily::Stock)
        .map(|owner| owner.underlying_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract_underlying_map::LegIds;
    use crate::depth200_atm::SwitchReason;
    use crate::depth200_candidates::{DEPTH_200_SOCKET_BUDGET, Depth200Candidate};
    use crate::depth200_ranked_steer::plan_ranked_minute;
    use proptest::prelude::*;
    use tickvault_common::types::ExchangeSegment;

    const FNO: ExchangeSegment = ExchangeSegment::NseFno;
    const HOLD: Duration = Duration::from_secs(DEPTH200_MIN_HOLD_SECS);

    fn inst(id: u64) -> SubscribeInstrument {
        SubscribeInstrument {
            security_id: id,
            segment: FNO,
        }
    }

    fn candidate(id: u64) -> Depth200Candidate {
        Depth200Candidate {
            security_id: id,
            segment: FNO,
            underlying_id: id,
            window_lots_milli: 1_000,
        }
    }

    /// Test eligibility: every contract is eligible and is its own
    /// underlying (matching `candidate`).
    fn own(instrument: &SubscribeInstrument) -> Option<u64> {
        Some(instrument.security_id)
    }

    fn swap(socket_index: usize, old: u64, new: u64) -> PlannedSwap {
        PlannedSwap {
            socket_index,
            old: inst(old),
            new: inst(new),
            reason: SwitchReason::VolumeRankChanged,
        }
    }

    /// Book with socket 0 stamped as holding contract 2 (swapped in over 1)
    /// at `t0`.
    fn book_with_swap_in(t0: Instant) -> (Depth200HoldBook, Vec<Option<SubscribeInstrument>>) {
        let mut book = Depth200HoldBook::new(1);
        let held = vec![Some(inst(2))];
        book.observe(&[Some(inst(1))]);
        let stamped = book.note_sent_swaps(&[swap(0, 1, 2)], |i| held[i], t0);
        assert_eq!(stamped, 1);
        (book, held)
    }

    #[test]
    fn test_depth200_min_hold_keeps_a_contract_for_600s() {
        let t0 = Instant::now();
        let (mut book, held) = book_with_swap_in(t0);
        // The ranking has moved on: contract 2 is off it, 3 is waiting.
        let ranked = [candidate(3)];
        for secs in [0u64, 60, 300, 540, 599] {
            book.observe(&held);
            let mask = book.locked_mask(&held, t0 + Duration::from_secs(secs), own);
            assert_eq!(mask, vec![Some(2)], "held at {secs} s");
            let d = plan_ranked_minute(&held, &ranked, &mask);
            assert!(d.is_quiet(), "no swap off at {secs} s");
            assert_eq!(d.min_hold, 1);
            assert_eq!(d.capped, 0, "a held socket is not a cap refusal");
            assert_eq!(d.held_back, 1, "the waiting candidate is the hold's");
            assert_eq!(d.unplaced, 0, "not reported as socket_empty");
        }
    }

    #[test]
    fn test_depth200_min_hold_releases_after_600s() {
        let t0 = Instant::now();
        let (mut book, held) = book_with_swap_in(t0);
        let ranked = [candidate(3)];
        for secs in [600u64, 601, 660, 3_600] {
            book.observe(&held);
            let mask = book.locked_mask(&held, t0 + Duration::from_secs(secs), own);
            assert_eq!(mask, vec![None], "released at {secs} s");
            let d = plan_ranked_minute(&held, &ranked, &mask);
            assert_eq!(d.swaps.len(), 1);
            assert_eq!(d.swaps[0].new, inst(3));
            assert_eq!(d.min_hold, 0);
        }
    }

    #[test]
    fn test_depth200_min_hold_never_blocks_an_empty_socket() {
        let t0 = Instant::now();
        let (mut book, _) = book_with_swap_in(t0);
        // The socket's contract went away (an empty slot, e.g. after a probe).
        let held = vec![None];
        book.observe(&held);
        assert_eq!(book.locked_mask(&held, t0, own), vec![None]);
        // A fresh book on a dial-placed (never swapped-in) pool holds nothing
        // either, so the first ranked minute plans exactly as without a hold.
        let fresh = Depth200HoldBook::new(DEPTH_200_SOCKET_BUDGET);
        let dial: Vec<Option<SubscribeInstrument>> = vec![
            Some(inst(901)),
            None,
            Some(inst(902)),
            None,
            Some(inst(903)),
        ];
        let mask = fresh.locked_mask(&dial, t0, own);
        assert_eq!(mask, vec![None; DEPTH_200_SOCKET_BUDGET]);
        let ranked = [candidate(1), candidate(2), candidate(3)];
        assert_eq!(
            plan_ranked_minute(&dial, &ranked, &mask),
            plan_ranked_minute(&dial, &ranked, &[])
        );
    }

    #[test]
    fn test_depth200_min_hold_drops_an_ineligible_contract() {
        let t0 = Instant::now();
        let (mut book, held) = book_with_swap_in(t0);
        book.observe(&held);
        let now = t0 + Duration::from_secs(60);
        // Contract 2 is no longer eligible (expired, not tradable): not held.
        let mask = book.locked_mask(&held, now, |i| {
            (i.security_id != 2).then_some(i.security_id)
        });
        assert_eq!(mask, vec![None]);
        let d = plan_ranked_minute(&held, &[candidate(3)], &mask);
        assert_eq!(d.swaps.len(), 1);
        // Through the real eligibility map: an empty map holds nothing; a map
        // holding contract 2 as a stock option holds it.
        let empty = ContractUnderlyingMap::new();
        let mask = book.locked_mask(&held, now, |i| depth200_held_underlying(&empty, i));
        assert_eq!(mask, vec![None]);
        let map = ContractUnderlyingMap::new();
        map.publish_from_legs(&[LegIds {
            contract_security_id: 2,
            underlying_security_id: 77,
            contract_segment: FNO,
            family: OptionFamily::Stock,
            lot_size: 50,
        }]);
        let mask = book.locked_mask(&held, now, |i| depth200_held_underlying(&map, i));
        assert_eq!(mask, vec![Some(77)], "locked, carrying its underlying");
    }

    #[test]
    fn test_depth200_held_underlying_needs_a_stock_option_in_todays_map() {
        let map = ContractUnderlyingMap::new();
        map.publish_from_legs(&[
            LegIds {
                contract_security_id: 10,
                underlying_security_id: 100,
                contract_segment: FNO,
                family: OptionFamily::Stock,
                lot_size: 50,
            },
            LegIds {
                contract_security_id: 11,
                underlying_security_id: 13,
                contract_segment: FNO,
                family: OptionFamily::Index,
                lot_size: 75,
            },
        ]);
        assert_eq!(depth200_held_underlying(&map, &inst(10)), Some(100));
        assert_eq!(
            depth200_held_underlying(&map, &inst(11)),
            None,
            "index option"
        );
        assert_eq!(depth200_held_underlying(&map, &inst(12)), None, "absent");
        let other_segment = SubscribeInstrument {
            security_id: 10,
            segment: ExchangeSegment::NseEquity,
        };
        assert_eq!(
            depth200_held_underlying(&map, &other_segment),
            None,
            "same id, other segment (I-P1-11)"
        );
    }

    #[test]
    fn test_depth200_hold_book_observe_restores_the_prior_swap_in_on_a_revert() {
        let t0 = Instant::now();
        let mut book = Depth200HoldBook::new(1);
        // Contract 1 swapped in at t0.
        book.observe(&[Some(inst(9))]);
        book.note_sent_swaps(&[swap(0, 9, 1)], |_| Some(inst(1)), t0);
        // At t0+700 s, 1 -> 2 is queued, then refused: the reconcile reverts.
        let t1 = t0 + Duration::from_secs(700);
        book.note_sent_swaps(&[swap(0, 1, 2)], |_| Some(inst(2)), t1);
        let reverted = [Some(inst(1))];
        book.observe(&reverted);
        // Contract 1 keeps its ORIGINAL swap-in (t0), so it is not re-held.
        assert_eq!(book.locked_mask(&reverted, t1, own), vec![None]);
        // A hold changed behind the loop's back reads as unstamped.
        book.observe(&[Some(inst(5))]);
        assert_eq!(book.locked_mask(&[Some(inst(5))], t1, own), vec![None]);
    }

    /// Reviewer finding (LOW): the test above passes even if `observe` CLEARED
    /// the slot on a revert, because 700 s is past the hold either way. Here
    /// the reverted contract is still inside its hold: A swapped in at t0,
    /// A -> B queued at t0+60 (as if A had turned ineligible), refused and
    /// reverted. A must get its ORIGINAL t0 swap-in back: locked at t0+120,
    /// released at t0+600. A cleared slot would read unlocked at t0+120; a
    /// fresh stamp would read locked at t0+600.
    #[test]
    fn test_depth200_hold_book_revert_restores_the_original_swap_in_inside_the_hold() {
        const A: u64 = 1;
        const B: u64 = 2;
        let t0 = Instant::now();
        let mut book = Depth200HoldBook::new(1);
        book.observe(&[Some(inst(9))]);
        assert_eq!(
            book.note_sent_swaps(&[swap(0, 9, A)], |_| Some(inst(A)), t0),
            1
        );
        book.observe(&[Some(inst(A))]);
        let t_swap = t0 + Duration::from_secs(60);
        assert_eq!(
            book.note_sent_swaps(&[swap(0, A, B)], |_| Some(inst(B)), t_swap),
            1
        );
        // The connection refused A -> B: the reconcile reverts the hold to A.
        let reverted = [Some(inst(A))];
        book.observe(&reverted);
        assert_eq!(
            book.locked_mask(&reverted, t0 + Duration::from_secs(120), own),
            vec![Some(A)],
            "A keeps its t0 swap-in, so it is still held at t0+120"
        );
        book.observe(&reverted);
        assert_eq!(
            book.locked_mask(&reverted, t0 + Duration::from_secs(600), own),
            vec![None],
            "released at t0+600, counted from the ORIGINAL swap-in"
        );
    }

    #[test]
    fn test_depth200_hold_book_note_sent_swaps_skips_a_swap_that_was_not_queued() {
        let t0 = Instant::now();
        let mut book = Depth200HoldBook::new(2);
        let held = [Some(inst(1)), Some(inst(4))];
        book.observe(&held);
        // Socket 0's send failed (held unchanged); socket 1's went out.
        let swaps = [swap(0, 1, 2), swap(1, 4, 5)];
        let after = [Some(inst(1)), Some(inst(5))];
        assert_eq!(book.note_sent_swaps(&swaps, |i| after[i], t0), 1);
        book.observe(&after);
        assert_eq!(book.locked_mask(&after, t0, own), vec![None, Some(5)]);
        // A swap for a socket past the book's length still records.
        assert_eq!(
            book.note_sent_swaps(&[swap(3, 6, 7)], |_| Some(inst(7)), t0),
            1
        );
    }

    #[test]
    fn test_depth200_hold_book_locked_mask_is_false_for_a_mismatched_or_short_book() {
        let t0 = Instant::now();
        let (book, _) = book_with_swap_in(t0);
        // Socket 0 holds something other than its stamp; socket 1 has no slot.
        let held = [Some(inst(8)), Some(inst(2))];
        assert_eq!(book.locked_mask(&held, t0, own), vec![None, None]);
        assert!(book.locked_mask(&[], t0, own).is_empty());
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]

        /// Random rankings over 40 minutes, random dial placements and random
        /// refused swaps, with contracts SHARING underlyings (30 ids over 7
        /// names; each ranking distinct by underlying, as the ranker
        /// guarantees): no socket changes contract within 600 s of its last
        /// swap-in, the mask is exactly "swapped in under 600 s ago" and
        /// carries the held contract's underlying, an empty socket is never
        /// held, the hold never puts a second socket on a locked socket's
        /// underlying, and on the first minute the hold blocks nothing (the
        /// plan equals the plan without a hold).
        #[test]
        fn proptest_depth200_min_hold_no_socket_changes_twice_within_600s(
            dial in proptest::collection::vec(
                proptest::option::of(0u64..50),
                DEPTH_200_SOCKET_BUDGET,
            ),
            rankings in proptest::collection::vec(
                proptest::collection::vec(1u64..=30, 0..=20),
                1..=40,
            ),
            refusals in proptest::collection::vec(any::<bool>(), 256),
        ) {
            let t0 = Instant::now();
            // Ranked contracts share 7 underlyings; a dial contract (>= 1,000)
            // is its own underlying, so it never shares a name.
            let name_of = |id: u64| if id >= 1_000 { id } else { 100 + id % 7 };
            let named = |id: u64| Depth200Candidate {
                underlying_id: name_of(id),
                ..candidate(id)
            };
            // Dial contracts are distinct per socket and off every ranking.
            let mut held: Vec<Option<SubscribeInstrument>> = dial
                .iter()
                .enumerate()
                .map(|(i, raw)| raw.map(|r| inst(1_000 + (i as u64) * 100 + r)))
                .collect();
            // The model's own record of each socket's effective swap-in.
            let mut swap_in: Vec<Option<Instant>> = vec![None; DEPTH_200_SOCKET_BUDGET];
            let mut reverts: Vec<(usize, SubscribeInstrument, Option<Instant>)> = Vec::new();
            let mut book = Depth200HoldBook::new(DEPTH_200_SOCKET_BUDGET);
            let mut refusal_index = 0usize;

            for (minute, raw) in rankings.iter().enumerate() {
                let now = t0 + Duration::from_secs(60 * minute as u64);
                // The reconcile: a refused swap reverts to the old contract.
                for (i, old, prev) in reverts.drain(..) {
                    held[i] = Some(old);
                    swap_in[i] = prev;
                }
                book.observe(&held);

                // Distinct by underlying, best first: the ranker's guarantee.
                let mut ids: Vec<u64> = Vec::new();
                for id in raw {
                    if !ids.iter().any(|&kept| name_of(kept) == name_of(*id)) {
                        ids.push(*id);
                    }
                }
                let ranked: Vec<Depth200Candidate> = ids.iter().map(|&i| named(i)).collect();

                let mask = book.locked_mask(&held, now, |i| Some(name_of(i.security_id)));
                prop_assert_eq!(mask.len(), DEPTH_200_SOCKET_BUDGET);
                for (i, ((locked, now_held), since)) in
                    mask.iter().zip(&held).zip(&swap_in).enumerate()
                {
                    let expected = now_held.is_some()
                        && since.is_some_and(|t| now.duration_since(t) < HOLD);
                    prop_assert_eq!(locked.is_some(), expected, "socket {} minute {}", i, minute);
                    if let (Some(name), Some(instrument)) = (locked, now_held) {
                        prop_assert_eq!(*name, name_of(instrument.security_id));
                    }
                    if now_held.is_none() {
                        prop_assert!(locked.is_none(), "an empty socket is never held");
                    }
                }

                let d = plan_ranked_minute(&held, &ranked, &mask);
                if minute == 0 {
                    prop_assert_eq!(&d, &plan_ranked_minute(&held, &ranked, &[]));
                }
                for s in &d.swaps {
                    let i = s.socket_index;
                    prop_assert_eq!(held[i], Some(s.old));
                    // The hold never puts a second socket on a locked
                    // socket's underlying: that socket keeps the name.
                    let new_name = name_of(s.new.security_id);
                    for (j, lock) in mask.iter().enumerate() {
                        prop_assert!(
                            *lock != Some(new_name),
                            "socket {} took underlying {} that locked socket {} keeps",
                            i,
                            new_name,
                            j
                        );
                    }
                    if let Some(t) = swap_in[i] {
                        prop_assert!(
                            now.duration_since(t) >= HOLD,
                            "socket {} changed {:?} after its swap-in",
                            i,
                            now.duration_since(t)
                        );
                    }
                    let prev = swap_in[i];
                    held[i] = Some(s.new);
                    swap_in[i] = Some(now);
                    let refused = refusals[refusal_index % refusals.len()];
                    refusal_index = refusal_index.saturating_add(1);
                    if refused {
                        reverts.push((i, s.old, prev));
                    }
                }
                let stamped =
                    book.note_sent_swaps(&d.swaps, |i| held.get(i).copied().flatten(), now);
                prop_assert_eq!(stamped, d.swaps.len());
            }
        }
    }
}
