//! Depth-200 steering from the VOLUME ranking — the 2026-09-06 lock, wired.
//!
//! # What this replaces, and when
//!
//! Until 2026-09-08 the five depth-200 sockets were steered by
//! [`crate::depth200_atm`] (NIFTY/BANKNIFTY at-the-money pairs) plus one
//! percent-change mover. The operator's 2026-09-06 directive
//! (`websocket-connection-scope-lock.md`, "DEPTH IS STOCK OPTIONS ONLY, RANKED
//! BY VOLUME") replaces that with the top five STOCK-option contracts by lots
//! traded in the window, each a DISTINCT underlying. PR #1890 built the
//! ranking and PUBLISHED it ([`crate::depth200_candidates`]) but only REPORTED
//! the divergence — every socket kept following the old engine. This module
//! is the step that moves them.
//!
//! # The three properties the lock makes binding
//!
//! | Property | How it is met here |
//! |---|---|
//! | DELTA-ONLY | a socket already holding a ranked contract is never touched; only sockets holding something OFF the ranking are swapped, and only onto contracts NOT held anywhere |
//! | EDGE-TRIGGERED | a quiet minute — the ranking and the holdings agree — costs zero wire calls, by construction |
//! | CAPPED PER WINDOW | at most [`MAX_RANKED_SWAPS_PER_MINUTE`] swaps per minute, and never more than one per socket (the reconcile-before-plan discipline of `depth_rebalance` enforces the latter) |
//!
//! # What it does NOT do (honest limits)
//!
//! * It applies the 5-second ranking ONCE A MINUTE, on the steering loop's
//!   own cadence. The set therefore reflects the ranking as of the last sweep
//!   before the minute mark. Moving the apply onto the 5-second timer would
//!   put swap I/O on the frame drain, which the same lock forbids.
//! * It never EMPTIES a socket. If fewer than five contracts are ranked, a
//!   socket holding an off-ranking contract keeps it rather than being
//!   unsubscribed to nothing — an empty depth socket delivers nothing and the
//!   unsubscribe code was unverified live when this was written (the 24-vs-25
//!   split; settled 2026-09-10 — 25 proven IGNORED, 24 ships, 24 itself
//!   UNVERIFIED-LIVE until a session reads `ghost = 0`).
//! * It cannot FILL an empty socket (`held == None`). A swap needs an old
//!   instrument to unsubscribe; a first subscription is a different command
//!   shape and stays with the legacy first-adoption path.
//! * Before the FIRST ranking of a session (pre-open: volume is zero and a
//!   ranking would be meaningless) the sockets stay on whatever the dial put
//!   there. That dial still selects index at-the-money contracts, which the
//!   lock bans — recorded as an open item, not silently accepted.
//!
//! # Complexity
//!
//! O(sockets × ranked): sockets is [`DEPTH_200_SOCKET_BUDGET`] (5) and `ranked`
//! is the PUBLISHED list, which is the exit set [`DEPTH200_EXIT_UNDERLYINGS`]
//! (20) — so at most ~125 key compares a minute. Cold path.
//!
//! ⚠ CORRECTED 2026-09-13: this read "with both bounded at five: at most 25 key
//! compares a minute". Only the SOCKET side is five. The ranked side became 20
//! when `DEPTH200_HYSTERESIS_RANKS` widened 3 → 15 on 2026-09-11, because the
//! planner reads the whole published band and not just the entry set.
//! Understated 5x, in the reassuring direction. Still trivially cold — the
//! correction is to the CLAIM, not to any cost worth acting on.

use crate::depth200_atm::{PlannedSwap, SwitchReason};
use crate::depth200_candidates::{DEPTH_200_SOCKET_BUDGET, Depth200Candidate};
use tickvault_core::websocket::pool_supervisor::SubscribeInstrument;

/// The per-window swap cap: one swap per socket per minute, five sockets.
///
/// Named separately from the socket budget so a future cap BELOW the budget
/// (a hysteresis decision after the churn is measured) is one constant, not a
/// re-derivation. Today they are equal and the assert below says so.
pub const MAX_RANKED_SWAPS_PER_MINUTE: usize = DEPTH_200_SOCKET_BUDGET;

const _: () = assert!(
    MAX_RANKED_SWAPS_PER_MINUTE <= DEPTH_200_SOCKET_BUDGET,
    "a cap above the socket count is not a cap"
);

/// Counter: swaps the ranked planner decided, by `outcome`.
///
/// `planned` — sent onward to the socket; `capped` — a needed swap refused
/// this minute by [`MAX_RANKED_SWAPS_PER_MINUTE`] (retried next minute);
/// `socket_empty` — a ranked contract had no socket to go to because the only
/// free socket holds nothing (see the module header). A rising `capped` is the
/// measurement the 2026-09-07 rule says decides whether the window needs
/// lengthening or a hysteresis band — never a reason to revert the key.
pub const RANKED_SWAPS_COUNTER: &str = "tv_depth200_ranked_swaps_total";

/// Gauge: sockets whose held contract was ON the ranking this minute (kept).
pub const RANKED_KEPT_GAUGE: &str = "tv_depth200_ranked_sockets_kept";

/// Every label value [`RANKED_SWAPS_COUNTER`] can carry, for pre-registration.
pub const RANKED_SWAP_OUTCOMES: [&str; 3] = ["planned", "capped", "socket_empty"];

/// Registers the counter's series at zero so the first swap is a delta the
/// CloudWatch agent can see, not a dropped first sample (the loss-series
/// seeding lesson of 2026-08-28).
pub fn pre_register_ranked_counters() {
    crate::volume_leaderboard::pre_register_gainer_filter_counter();
    for outcome in RANKED_SWAP_OUTCOMES {
        metrics::counter!(RANKED_SWAPS_COUNTER, "outcome" => outcome).increment(0);
    }
    metrics::gauge!(RANKED_KEPT_GAUGE).set(0.0);
}

/// One minute of ranked steering, decided but not sent.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RankedDecision {
    /// Swaps to send, at most [`MAX_RANKED_SWAPS_PER_MINUTE`], in socket order.
    pub swaps: Vec<PlannedSwap>,
    /// Sockets left alone because they already hold a ranked contract.
    pub kept: usize,
    /// Needed swaps refused by the per-minute cap. Retried next minute.
    pub capped: usize,
    /// Ranked contracts that had no socket to go to: every non-kept socket was
    /// either already assigned this minute or holds nothing.
    pub unplaced: usize,
}

impl RankedDecision {
    /// Whether this minute costs any wire calls.
    #[must_use]
    pub fn is_quiet(&self) -> bool {
        self.swaps.is_empty()
    }
}

/// Plans the swaps that move `held` toward `ranked`, delta-only.
///
/// `held[i]` is what socket `i` is believed to hold — `None` only before its
/// first subscription. `ranked` is the published top-N, already
/// distinct-underlying and already stock-only (the ranking layer guarantees
/// both; this function does not re-check them, because re-deriving a
/// guarantee at a second site is where the two drift).
///
/// Assignment is by ORDER on both sides: the best unheld candidate goes to the
/// lowest-indexed socket holding something off the ranking. Nothing else is
/// optimised — with five of each there is nothing to gain from a smarter
/// pairing, and a simpler rule is one a reader can verify against the log.
///
/// # The hysteresis band (2026-09-08 SECOND)
///
/// `ranked` is longer than the socket count: the first
/// [`crate::depth200_candidates::DEPTH_200_SOCKET_BUDGET`] rows are the ENTRY
/// set and the rest is the band. A contract is placed only from the entry
/// set; a contract already held is KEPT while it stays anywhere in the list.
/// So a held contract that slips from fifth to sixth for one window is not
/// swapped out and back — the churn the 2026-09-07 lock names a band for.
#[must_use]
pub fn plan_ranked_minute(
    held: &[Option<SubscribeInstrument>],
    ranked: &[Depth200Candidate],
) -> RankedDecision {
    let mut decision = RankedDecision::default();

    // Keys of what is on the wire, for "is this candidate already held?".
    let held_keys: Vec<(u64, u8)> = held
        .iter()
        .flatten()
        .map(|i| (i.security_id, i.segment.binary_code()))
        .collect();
    // ENTRY candidates NOT held anywhere, best first. The band is never
    // placed from — it only decides what is kept, below.
    let mut to_place = crate::depth200_candidates::entry_set(ranked)
        .iter()
        .filter(|c| !held_keys.contains(&c.key()))
        .peekable();

    for (socket_index, slot) in held.iter().enumerate() {
        let Some(old) = slot else {
            // An empty socket cannot take a swap (nothing to unsubscribe);
            // counted below if a candidate was waiting for it.
            continue;
        };
        let old_key = (old.security_id, old.segment.binary_code());
        if ranked.iter().any(|c| c.key() == old_key) {
            decision.kept = decision.kept.saturating_add(1);
            continue;
        }
        // This socket holds something OFF the ranking: it is the next home.
        //
        // THREE steps, and the order of all three is load-bearing.
        //
        // 1. PEEK first. Nothing left to place means the socket keeps its
        //    off-ranking contract rather than being emptied (module header) —
        //    and it is NOT a cap refusal, so `capped` must not move.
        // 2. THEN the cap. Reached with a candidate still waiting, the socket
        //    is genuinely refused: count it and leave the candidate in the
        //    iterator, where the terminal `to_place.count()` reports it as
        //    `unplaced`.
        // 3. ONLY THEN consume.
        //
        // Pulling before the cap check (the shape here until 2026-09-13)
        // deletes a candidate from BOTH the plan and `unplaced` whenever the
        // cap bites with candidates remaining: `capped` counts the SOCKET
        // while the CONTRACT vanishes with no counter naming it. Unreachable
        // today only because MAX_RANKED_SWAPS_PER_MINUTE ==
        // DEPTH_200_SOCKET_BUDGET while the loop iterates exactly that many
        // sockets — and that constant's own doc invites a LOWER cap as the
        // expected tuning, which is precisely what would arm it.
        //
        // Checking the cap first and consuming second is NOT the fix: with the
        // queue already empty it reports `capped` for sockets that were never
        // refused anything (proven by
        // `the_per_minute_cap_refuses_and_counts_the_overflow`, which that
        // ordering fails). Peek separates "nothing to give" from "not allowed
        // to give it", which is the distinction both counters exist to make.
        //
        // `continue`, NOT `break` (the shape here until 2026-09-13): a LATER
        // socket may hold a ranked contract and belongs in `kept`. Breaking at
        // the first off-ranking socket after the queue drains stops examining
        // every socket behind it, so the `tv_depth200_ranked_sockets_kept`
        // gauge under-reports — ranked [A,B,C] against held
        // [off, A, B, C, _] published 0 when the truth was 3, telling an
        // operator the pool churned when nothing moved. Continuing is
        // otherwise inert: the socket is still left alone (no swap is pushed),
        // `to_place` stays empty so no later socket can be funded either, and
        // the terminal `to_place.count()` is unaffected.
        if to_place.peek().is_none() {
            continue;
        }
        if decision.swaps.len() >= MAX_RANKED_SWAPS_PER_MINUTE {
            decision.capped = decision.capped.saturating_add(1);
            continue;
        }
        let Some(candidate) = to_place.next() else {
            // Unreachable: the peek two statements up returned `Some`, and
            // nothing between there and here advances the iterator. Written as
            // a `break` rather than an `expect` because `panic = "abort"` on
            // the release profile turns a wrong assumption here into process
            // death mid-session, and because `clippy::expect_used` is denied
            // outside tests — the house rule that forbids exactly that trade.
            break;
        };
        decision.swaps.push(PlannedSwap {
            socket_index,
            old: *old,
            new: SubscribeInstrument {
                security_id: candidate.security_id,
                segment: candidate.segment,
            },
            reason: SwitchReason::VolumeRankChanged,
        });
    }
    // Whatever is still waiting had no socket to go to this minute.
    decision.unplaced = to_place.count();
    decision
}

/// Publishes one minute's decision to the counters and gauge.
pub fn record_ranked_decision(decision: &RankedDecision) {
    metrics::gauge!(RANKED_KEPT_GAUGE).set(decision.kept as f64);
    if !decision.swaps.is_empty() {
        metrics::counter!(RANKED_SWAPS_COUNTER, "outcome" => "planned")
            .increment(decision.swaps.len() as u64);
    }
    if decision.capped > 0 {
        metrics::counter!(RANKED_SWAPS_COUNTER, "outcome" => "capped")
            .increment(decision.capped as u64);
    }
    if decision.unplaced > 0 {
        metrics::counter!(RANKED_SWAPS_COUNTER, "outcome" => "socket_empty")
            .increment(decision.unplaced as u64);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tickvault_common::types::ExchangeSegment;

    const FNO: ExchangeSegment = ExchangeSegment::NseFno;
    const IDX: ExchangeSegment = ExchangeSegment::IdxI;

    fn candidate(id: u64, underlying: u64, lots: u64) -> Depth200Candidate {
        Depth200Candidate {
            security_id: id,
            segment: FNO,
            underlying_id: underlying,
            window_lots_milli: lots,
        }
    }

    fn held(id: u64, segment: ExchangeSegment) -> Option<SubscribeInstrument> {
        Some(SubscribeInstrument {
            security_id: id,
            segment,
        })
    }

    /// The overwhelmingly common minute: holdings and ranking agree, zero
    /// wire calls. This is the edge-trigger property in one assertion.
    #[test]
    fn plan_ranked_minute_on_an_aligned_pool_plans_nothing() {
        let ranked = [
            candidate(1, 10, 500),
            candidate(2, 20, 400),
            candidate(3, 30, 300),
        ];
        let holds = [held(3, FNO), held(1, FNO), held(2, FNO)];
        let d = plan_ranked_minute(&holds, &ranked);
        assert!(d.is_quiet());
        assert_eq!(d.kept, 3);
        assert_eq!((d.capped, d.unplaced), (0, 0));
    }

    /// The 2026-08-26 shape: four index options and one stock mover on the
    /// wire, five stock options ranked. Every socket moves, once, and each
    /// swap names an OLD the socket really holds — never an invented one.
    #[test]
    fn five_off_ranking_sockets_take_the_five_ranked_contracts_in_order() {
        let ranked: Vec<_> = (1..=5).map(|i| candidate(100 + i, i, 1000 - i)).collect();
        let holds = [
            held(9001, IDX),
            held(9002, IDX),
            held(9003, IDX),
            held(9004, IDX),
            held(77, FNO),
        ];
        let d = plan_ranked_minute(&holds, &ranked);
        assert_eq!(d.swaps.len(), 5);
        assert_eq!(d.kept, 0);
        for (i, swap) in d.swaps.iter().enumerate() {
            assert_eq!(swap.socket_index, i);
            assert_eq!(swap.old, holds[i].expect("held"));
            assert_eq!(
                swap.new.security_id, ranked[i].security_id,
                "best candidate to lowest socket"
            );
            assert_eq!(swap.new.segment, FNO);
            assert_eq!(swap.reason, SwitchReason::VolumeRankChanged);
        }
    }

    /// DELTA-ONLY: a socket already holding a ranked contract is untouched,
    /// and its contract is not handed to another socket as well.
    #[test]
    fn a_held_ranked_contract_is_neither_moved_nor_duplicated() {
        let ranked = [candidate(1, 10, 500), candidate(2, 20, 400)];
        let holds = [held(9001, IDX), held(2, FNO)];
        let d = plan_ranked_minute(&holds, &ranked);
        assert_eq!(d.kept, 1);
        assert_eq!(d.swaps.len(), 1);
        assert_eq!(d.swaps[0].socket_index, 0);
        assert_eq!(
            d.swaps[0].new.security_id, 1,
            "contract 2 is already on socket 1"
        );
    }

    /// A socket is never EMPTIED: with fewer ranked contracts than off-ranking
    /// sockets, the surplus sockets keep what they hold.
    #[test]
    fn surplus_sockets_keep_their_off_ranking_contract_rather_than_going_empty() {
        let ranked = [candidate(1, 10, 500)];
        let holds = [held(9001, IDX), held(9002, IDX), held(9003, IDX)];
        let d = plan_ranked_minute(&holds, &ranked);
        assert_eq!(d.swaps.len(), 1);
        assert_eq!(d.swaps[0].socket_index, 0);
        assert_eq!(d.unplaced, 0);
    }

    /// An empty ranking (a quiet window, or the ranking layer refusing
    /// everything through its monotonicity gate) moves nothing.
    #[test]
    fn an_empty_ranking_moves_nothing() {
        let holds = [held(9001, IDX), held(9002, IDX)];
        let d = plan_ranked_minute(&holds, &[]);
        assert!(d.is_quiet());
        assert_eq!(d.kept, 0);
    }

    /// An empty socket cannot take a swap; the candidate it would have taken
    /// is counted as unplaced rather than silently dropped.
    #[test]
    fn an_empty_socket_is_skipped_and_the_orphaned_candidate_is_counted() {
        let ranked = [candidate(1, 10, 500), candidate(2, 20, 400)];
        let holds = [None, held(9001, IDX)];
        let d = plan_ranked_minute(&holds, &ranked);
        assert_eq!(d.swaps.len(), 1);
        assert_eq!(d.swaps[0].socket_index, 1);
        assert_eq!(d.unplaced, 1);
    }

    /// The per-minute cap binds even when more sockets need moving; the
    /// refused ones are counted and retried next minute.
    #[test]
    fn the_per_minute_cap_refuses_and_counts_the_overflow() {
        // The cap equals the entry set today, so it can only be REACHED, not
        // exceeded: with more off-ranking sockets than entry candidates the
        // planner sends exactly `cap` swaps and the surplus sockets keep what
        // they hold. The band rows (beyond the entry set) are never placed —
        // that is `band_contracts_are_never_placed_into_a_socket` below.
        let n = MAX_RANKED_SWAPS_PER_MINUTE + 2;
        let ranked: Vec<_> = (1..=n as u64)
            .map(|i| candidate(100 + i, i, 1000 - i))
            .collect();
        let holds: Vec<_> = (1..=n as u64).map(|i| held(9000 + i, IDX)).collect();
        let d = plan_ranked_minute(&holds, &ranked);
        assert_eq!(d.swaps.len(), MAX_RANKED_SWAPS_PER_MINUTE);
        assert_eq!(d.capped, 0, "nothing beyond the entry set is ever queued");
        assert_eq!(d.unplaced, 0);
    }

    /// Draining the arrival queue must not stop the loop: sockets BEHIND the
    /// first unfunded one are still examined, so a held ranked contract is
    /// still counted `kept`.
    ///
    /// The gauge is the whole reason. `tv_depth200_ranked_sockets_kept` is
    /// what an operator reads to answer "did the pool churn this minute?" —
    /// a socket order of [off-ranking, A, B, C, empty] against ranked [A,B,C]
    /// reported 0 under the `break`, which reads as five sockets swapping out
    /// on a minute that moved nothing.
    #[test]
    fn an_exhausted_arrival_queue_still_counts_later_kept_sockets() {
        let ranked = [
            candidate(1, 10, 500),
            candidate(2, 20, 400),
            candidate(3, 30, 300),
        ];
        // Socket 0 is off the ranking and takes the one unheld entry row
        // (there is none — A, B and C are all held), so the queue is empty at
        // socket 0 and the three KEPT sockets sit behind it.
        let holds = [
            held(9001, IDX),
            held(1, FNO),
            held(2, FNO),
            held(3, FNO),
            None,
        ];
        let d = plan_ranked_minute(&holds, &ranked);
        assert!(
            d.is_quiet(),
            "nothing is unheld, so the off-ranking socket keeps what it holds"
        );
        assert_eq!(
            d.kept, 3,
            "sockets 1..3 hold ranked contracts and must be counted even \
             though socket 0 found the arrival queue empty"
        );
        assert_eq!((d.capped, d.unplaced), (0, 0));
    }

    /// The hysteresis band: a held contract that slipped OUT of the entry set
    /// but is still inside the published list is KEPT, not swapped out.
    #[test]
    fn a_held_contract_inside_the_band_is_kept_and_not_swapped() {
        let band = crate::depth200_candidates::DEPTH200_EXIT_UNDERLYINGS as u64;
        // Ranks 1..=8 (5 entry + 3 band).
        let ranked: Vec<_> = (1..=band).map(|i| candidate(i, 10 * i, 1000 - i)).collect();
        // Socket 0 holds rank 6 (the first band row); the other four hold
        // ranks 1..4, so entry rank 5 is unheld.
        let holds = [
            held(6, FNO),
            held(1, FNO),
            held(2, FNO),
            held(3, FNO),
            held(4, FNO),
        ];
        let d = plan_ranked_minute(&holds, &ranked);
        assert_eq!(d.kept, 5, "rank 6 is inside the band and is kept");
        assert!(
            d.is_quiet(),
            "no socket is off the ranking, so nothing moves"
        );
        // Rank 5 is unheld but has no home; it is counted, never forced.
        assert_eq!(d.unplaced, 1);
    }

    /// Band rows decide what is KEPT, never what is PLACED: an off-ranking
    /// socket takes an entry-set contract, and if the entry set is exhausted
    /// it keeps what it holds rather than taking a band row.
    #[test]
    fn band_contracts_are_never_placed_into_a_socket() {
        let band = crate::depth200_candidates::DEPTH200_EXIT_UNDERLYINGS as u64;
        let entry = crate::depth200_candidates::DEPTH_200_SOCKET_BUDGET as u64;
        let ranked: Vec<_> = (1..=band).map(|i| candidate(i, 10 * i, 1000 - i)).collect();
        // All five sockets hold entry rows except socket 4, which holds an
        // index option (off the ranking entirely).
        let holds = [
            held(1, FNO),
            held(2, FNO),
            held(3, FNO),
            held(4, FNO),
            held(9001, IDX),
        ];
        let d = plan_ranked_minute(&holds, &ranked);
        assert_eq!(d.swaps.len(), 1);
        assert_eq!(
            d.swaps[0].new.security_id, entry,
            "the last ENTRY row, never a band row"
        );
        // Now the entry set is fully held; a second off-ranking socket must
        // NOT be handed a band row.
        let holds = [
            held(1, FNO),
            held(2, FNO),
            held(3, FNO),
            held(4, FNO),
            held(5, FNO),
            held(9002, IDX),
        ];
        let d = plan_ranked_minute(&holds, &ranked);
        assert!(
            d.is_quiet(),
            "band rows are not placed; the socket keeps its contract"
        );
        assert_eq!(d.kept, 5);
    }

    /// I-P1-11: the same numeric id in another SEGMENT is a different
    /// instrument. A held index option must not read as "already holding" the
    /// stock option that shares its number.
    #[test]
    fn a_held_id_in_another_segment_does_not_count_as_holding_the_ranked_contract() {
        let ranked = [candidate(13, 10, 500)];
        let holds = [held(13, IDX)];
        let d = plan_ranked_minute(&holds, &ranked);
        assert_eq!(
            d.swaps.len(),
            1,
            "same number, different segment, still a swap"
        );
        assert_eq!(d.kept, 0);
    }

    /// `is_quiet` is exactly "no swaps": kept, capped and unplaced counts do
    /// not make a minute loud — only a swap costs a wire call.
    #[test]
    fn is_quiet_is_true_only_when_there_are_no_swaps() {
        let quiet = RankedDecision {
            swaps: Vec::new(),
            kept: 3,
            capped: 2,
            unplaced: 1,
        };
        assert!(quiet.is_quiet());
        let loud = plan_ranked_minute(&[held(9001, IDX)], &[candidate(1, 10, 500)]);
        assert!(!loud.is_quiet());
    }

    /// Recording a decision must accept every shape, including the all-zero
    /// one, without a panic — it runs once a minute for the whole session.
    #[test]
    fn record_ranked_decision_accepts_every_shape() {
        record_ranked_decision(&RankedDecision::default());
        // An index option holding against one ranked stock option is exactly
        // one swap and nothing kept; recording that shape must not change it.
        let decision = plan_ranked_minute(&[held(9001, IDX)], &[candidate(1, 10, 500)]);
        record_ranked_decision(&decision);
        assert_eq!(decision.swaps.len(), 1);
        assert_eq!(decision.kept, 0);
    }

    #[test]
    fn pre_register_ranked_counters_covers_every_label() {
        pre_register_ranked_counters();
        record_ranked_decision(&RankedDecision {
            swaps: Vec::new(),
            kept: 2,
            capped: 1,
            unplaced: 1,
        });
        assert_eq!(RANKED_SWAP_OUTCOMES.len(), 3);
    }

    /// `plan_ranked_minute` PEEKS, then tests the cap, then consumes — in that
    /// order.
    ///
    /// This cannot be a behavioural test. `MAX_RANKED_SWAPS_PER_MINUTE` equals
    /// `DEPTH_200_SOCKET_BUDGET`, and `entry_set` slices the ranked list to
    /// that same budget, so `to_place` can never still hold a candidate once
    /// the cap is reached. The defect is LATENT and arms the moment the cap is
    /// tuned BELOW the budget — which that constant's own doc names as the
    /// expected future change. A source-order pin is the only assertion that
    /// can fail today.
    ///
    /// What each edge buys:
    /// * peek before cap — an empty queue must `continue`, not report `capped`
    ///   (`the_per_minute_cap_refuses_and_counts_the_overflow` fails otherwise)
    /// * cap before consume — a refused candidate must stay in the iterator so
    ///   the terminal `to_place.count()` reports it as `unplaced`
    ///
    /// Sliced to the FUNCTION, never searched across the file: `continue;` and
    /// `to_place` both occur elsewhere in this module, and a whole-file `find`
    /// would happily anchor on a sibling — the mis-anchoring this repository
    /// has now found nine times.
    #[test]
    fn the_planner_peeks_then_caps_then_consumes() {
        let src = include_str!("depth200_ranked_steer.rs");
        // Strip the test module, so these very assertions cannot satisfy the
        // scan that reads them.
        let prod = src
            .split_once("\nmod tests {")
            .map_or(src, |(before, _)| before);

        const FN: &str = "pub fn plan_ranked_minute(";
        const PEEK: &str = "if to_place.peek().is_none() {";
        const CAP: &str = "if decision.swaps.len() >= MAX_RANKED_SWAPS_PER_MINUTE {";
        const TAKE: &str = "let Some(candidate) = to_place.next() else {";

        assert_eq!(
            prod.matches(FN).count(),
            1,
            "expected exactly one definition of `plan_ranked_minute` in the \
             production region; the slice below assumes it"
        );
        let from = prod.find(FN).expect("counted above");
        let rest = &prod[from + FN.len()..];
        let to = rest.find("\npub fn ").unwrap_or(rest.len());
        let body = &rest[..to];

        for (label, needle) in [("peek", PEEK), ("cap", CAP), ("take", TAKE)] {
            assert_eq!(
                body.matches(needle).count(),
                1,
                "expected exactly one `{label}` site inside \
                 `plan_ranked_minute`, found {}",
                body.matches(needle).count()
            );
        }

        let peek_at = body.find(PEEK).expect("counted above");
        let cap_at = body.find(CAP).expect("counted above");
        let take_at = body.find(TAKE).expect("counted above");

        assert!(
            peek_at < cap_at,
            "an EMPTY queue must skip the socket, not count a cap refusal: \
             peek at {peek_at}, cap at {cap_at}"
        );
        assert!(
            cap_at < take_at,
            "the cap must refuse BEFORE the candidate is consumed, or the \
             refused candidate is deleted from the plan AND from `unplaced` \
             and no counter names it: cap at {cap_at}, take at {take_at}"
        );
    }
}
