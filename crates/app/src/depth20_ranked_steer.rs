//! Depth-20 steering from the VOLUME ranking — the 2026-09-06 lock's other
//! half, wired.
//!
//! # What this replaces, and when
//!
//! Until 2026-09-08 the five depth-20 sockets ran the 2026-08-26 layout:
//! NIFTY and BANKNIFTY at-the-money ±12 on two sockets, percent-change movers
//! on three ([`crate::depth20_layout`]). The operator's 2026-09-06 directive
//! (`websocket-connection-scope-lock.md`, "DEPTH IS STOCK OPTIONS ONLY, RANKED
//! BY VOLUME") replaces that with the top 250 STOCK-option contracts by lots
//! traded in the window, gainers as the eligibility filter. PR #1890/#1891
//! wired the DEPTH-200 half ([`crate::depth200_ranked_steer`]) and recorded
//! depth-20 as BLOCKED: the ranking layer published only the top five, and
//! the 50-instrument socket needs a set diff rather than a one-for-one swap.
//! This module is both halves of that block.
//!
//! # The three properties the lock makes binding
//!
//! | Property | How it is met here |
//! |---|---|
//! | DELTA-ONLY | an instrument already held anywhere in the pool is never touched; only held instruments OFF the ranking are released, and only onto ranked contracts held NOWHERE |
//! | EDGE-TRIGGERED | a quiet minute — the ranking and the holdings agree — costs zero wire calls, by construction |
//! | CAPPED PER WINDOW | at most [`MAX_RANKED_DEPTH20_SWAPS_PER_SOCKET_PER_MINUTE`] swaps per socket per minute, pinned to the socket's command-channel depth so a plan can never ask for more than the wire can queue |
//!
//! # Why a set diff over the WHOLE pool, not a per-socket layout
//!
//! The five sockets are one pool of 250 slots. Handing each socket a fixed
//! rank band (socket 0 = ranks 1–50, …) would swap an instrument between
//! sockets every time it crossed a band edge — pure churn for a contract that
//! was ranked before and after. So the desired set is compared against the
//! UNION of what every socket holds: an arrival is a ranked contract held
//! nowhere, a departure is a held contract ranked nowhere, and each socket
//! funds its own departures with the best arrivals still unplaced. Which
//! socket carries which ranked contract is deliberately NOT a property the
//! ranking cares about.
//!
//! # What it does NOT do (honest limits)
//!
//! * It applies the 3-second ranking (5-second until 2026-09-23) ONCE A MINUTE, on the steering loop's
//!   own cadence, for the reason the depth-200 module gives: swap I/O on the
//!   5-second timer would sit on the frame drain, which the lock forbids.
//! * It never EMPTIES a socket and never SHRINKS one: every wire action is a
//!   swap, so a socket ends the minute holding exactly what it held in count.
//!   A departure with no arrival to fund it is kept, and counted.
//! * It cannot FILL an empty socket (`held.is_empty()`): a swap needs
//!   something to release. Such a socket is counted as left alone.
//! * A ranking of fewer than 250 contracts leaves the surplus holdings in
//!   place (unused departures). A ranking of MORE than the pool can hold
//!   leaves the tail unplaced, counted — the ranking is already capped at
//!   `TOP_VOLUME_RANK_PER_FAMILY` = 250 = the pool, so this is the
//!   partially-dialed-pool case, not the normal one.
//! * Before the FIRST ranking of a session the sockets stay on whatever the
//!   dial put there; the caller falls back to the legacy layout until then
//!   (see `depth_rebalance`), exactly as depth-200 does.
//!
//! # Complexity
//!
//! O(held + ranked) set construction plus O(held) pairing, both bounded by
//! the 250-slot pool: a few hundred `BTreeSet` operations once a minute, on
//! the steering task. Cold path, flagged in CLAUDE.md's O(1) table rather
//! than claimed O(1).

use crate::depth20_track::{Depth20Plan, Depth20SocketPlan};
use crate::depth200_candidates::{Depth200Candidate, NOT_YET_RANKED};
use arc_swap::ArcSwapOption;
use std::collections::BTreeSet;
use std::sync::Arc;
use tickvault_core::websocket::pool_supervisor::SubscribeInstrument;

/// What one stock name COSTS to rotate, in swaps.
///
/// A name is its spot, its nearest future and its ATM window both legs —
/// `slots_for_stock_name(DEPTH20_STOCK_ATM_STRIKES_EACH_SIDE)`, 23 today — and
/// `plan_depth20_minute` pairs each departure with an arrival, so replacing one
/// name is exactly that many swaps.
///
/// DERIVED, never a literal. A wider stock ladder must move this figure with
/// it, or the cap below silently stops being "one whole name" while still
/// claiming to be.
pub const DEPTH20_NAME_SWAP_COST: usize = crate::depth20_name_board::slots_for_stock_name(
    crate::depth20_name_board::DEPTH20_STOCK_ATM_STRIKES_EACH_SIDE,
);

/// The depth of a DEPTH-20 socket's swap command channel, as the frame stack
/// creates it. Pinned here so the per-minute cap below cannot drift above what
/// `try_send` can actually queue.
///
/// **Raised 4 → one whole name on 2026-09-13** (operator: *"why the fuck per
/// mintue depth 20 is not yet implemented"*; the grant is the 2026-09-11
/// (FOURTH) scope-lock section, whose condition 1 — "it ships WITH the name
/// board, never before it" — the same day's name-board wiring satisfied).
///
/// Four was the frame stack's own figure and it was right for the engine it was
/// written for: *"enough that a busy minute cannot block the sender, small
/// enough that a wedged connection surfaces as a refused `try_send` the caller
/// LOGS rather than as a queue that hides it"*. The volume-ranked engine's
/// healthy minute produced two swaps a socket. A NAME board's healthy minute
/// produces a whole name, and at a depth of four the wire took six minutes to
/// agree with a board that had already chosen correctly.
///
/// **The honest cost, and there is no shape that avoids it:** a queue that can
/// hold a name can hide a wedge for a name's worth of sends. The refusal now
/// arrives on the NEXT minute's first `try_send` instead of this minute's
/// fifth — still counted as `channel_full`, still logged, one minute late.
pub const DEPTH_SWAP_COMMAND_CHANNEL_DEPTH: usize = DEPTH20_NAME_SWAP_COST;

/// The depth of a DEPTH-200 socket's swap command channel — deliberately
/// UNCHANGED at four.
///
/// That pool swaps at most one instrument per socket per minute
/// (`MAX_RANKED_SWAPS_PER_MINUTE` is its five-socket budget, one each), so a
/// deeper queue buys it nothing and costs it the thing the depth is for: at
/// four, a wedged depth-200 connection surfaces after four minutes; at
/// twenty-four it would take twenty-four.
pub const DEPTH200_SWAP_COMMAND_CHANNEL_DEPTH: usize = 4;

/// The per-socket, per-minute swap cap — one whole name.
///
/// Each swap is an unsubscribe plus a subscribe, each bounded by the
/// supervisor's one-second wire budget, executed sequentially on the socket's
/// task. A whole name is therefore at most ~48 s of a socket's minute on the
/// wire, and exactly the number the command channel can hold — one more
/// `try_send` would be refused as `channel_full` anyway, so a cap above the
/// depth is not a cap.
///
/// **The ~48 s is a CEILING, not a measurement.** `SWAP_WIRE_BUDGET` is a
/// `timeout`; the real per-leg cost is a socket write and should be
/// sub-millisecond, which would put a full rotation near 50 ms. Nobody has
/// measured it — `tv_dhan_ws_swap_wire_ms` was built for exactly this question
/// and has never seen a live session. If a leg genuinely approaches its budget,
/// this cap comes down; the budget does not go up.
pub const MAX_RANKED_DEPTH20_SWAPS_PER_SOCKET_PER_MINUTE: usize = DEPTH_SWAP_COMMAND_CHANNEL_DEPTH;

/// How many ranked contracts the depth-20 pool ENTERS from: the operator's
/// "for depth 20 pick top 250", i.e. the pool's own instrument budget.
pub const DEPTH20_ENTRY_RANKS: usize = tickvault_common::constants::TOP_VOLUME_RANK_PER_FAMILY;

/// How far down the ranking a HELD contract may fall before it is released:
/// the hysteresis band the 2026-09-07 lock names as the remedy for a churning
/// board ("a longer window or a hysteresis band on entry/exit — NOT reverting
/// to cumulative").
///
/// Under the lots-in-window key the board is NOT monotonic: a contract at
/// rank 245 in one 5-second window is routinely at rank 260 in the next, and
/// without a band every such wobble at the boundary is an unsubscribe plus a
/// subscribe on a socket the operator's own envelope prices at up to two
/// seconds of wire time each. A contract must fall out of the top
/// `DEPTH20_EXIT_RANKS` to be released, and a socket is only refilled from the
/// top [`DEPTH20_ENTRY_RANKS`], so the subscribed set stays 250 while the
/// boundary stops flapping. 50 ranks (20%) is a DESIGN choice, not a
/// measurement: `tv_depth20_ranked_swaps_total` on the first live session is
/// the number that decides whether it is wide enough.
pub const DEPTH20_EXIT_RANKS: usize = DEPTH20_ENTRY_RANKS + 50;

const _: () = assert!(
    DEPTH20_EXIT_RANKS > DEPTH20_ENTRY_RANKS,
    "the exit band must sit BELOW the entry cut or it is no band at all"
);

/// The depth-20 entry band and the PERSISTENCE cut are separate numbers, and
/// this fails the build if anyone re-aliases them.
///
/// They were one constant until 2026-09-12. Removing the top-250 persistence
/// cut in place — the obvious way to do what the operator asked — would have
/// carried `DEPTH20_ENTRY_RANKS` with it and re-steered a LIVE subscription
/// set as a side effect of a storage change: the pool would have tried to
/// enter from a band of `usize::MAX`, and `DEPTH20_EXIT_RANKS = ENTRY + 50`
/// would have overflowed in the same expression. Nothing in the tree caught
/// that shape; this does.
const _: () = assert!(
    DEPTH20_ENTRY_RANKS != tickvault_common::constants::TOP_VOLUME_PERSIST_PER_FAMILY,
    "the depth-20 entry band has been re-aliased to the persistence cut. The \
     depth budget is the vendor's socket capacity (5 sockets x 50 instruments) \
     and does not move with how many rows we choose to STORE."
);
const _: () = assert!(
    DEPTH20_ENTRY_RANKS == 250,
    "the depth-20 entry band is the pool's instrument budget and is pinned at \
     250 by the vendor socket capacity, not by any persistence decision"
);

const _: () = assert!(
    MAX_RANKED_DEPTH20_SWAPS_PER_SOCKET_PER_MINUTE <= DEPTH_SWAP_COMMAND_CHANNEL_DEPTH,
    "a per-minute cap above the command channel depth asks for swaps the wire refuses"
);

// The cap must DRAIN inside the minute that planned it.
//
// Each swap is two sequential wire calls, each bounded by the supervisor's
// `SWAP_WIRE_BUDGET`. If the worst case cannot finish before the next steering
// iteration arrives, the cap is not a cap — it is a backlog with a number on
// it, and the next minute's `try_send` meets a queue that never emptied.
//
// 23 x 2 x 1 s = 46 s inside a 60 s interval. Raising the ladder, the budget or
// the cap far enough to break this fails the build rather than quietly
// producing a pool that is permanently one minute behind its own board.
const _: () = assert!(
    (MAX_RANKED_DEPTH20_SWAPS_PER_SOCKET_PER_MINUTE as u64)
        .saturating_mul(2)
        .saturating_mul(tickvault_core::websocket::pool_supervisor::SWAP_WIRE_BUDGET.as_secs())
        < crate::depth_rebalance::REBALANCE_INTERVAL_SECS,
    "a per-socket swap plan that cannot drain inside one steering interval is a \
     backlog, not a cap: lower the cap or raise the interval"
);

// One whole name in one minute, which is the 2026-09-13 operator requirement
// stated as an assertion rather than as a comment.
//
// Below this the wire takes more than one minute to agree with a board that
// already chose correctly — the exact gap the operator named. Above it the cap
// is buying nothing the board can use, since a name is the largest unit the
// board ever moves.
const _: () = assert!(
    MAX_RANKED_DEPTH20_SWAPS_PER_SOCKET_PER_MINUTE >= DEPTH20_NAME_SWAP_COST,
    "a cap below one name's swap cost cannot rotate a name in a minute, which is \
     what the depth-20 name board exists to do"
);

// Depth-200's channel keeps the frame stack's original wedge-signal figure.
// Raising it with depth-20's would delay that pool's `channel_full` by six
// times for a pool that swaps at most once per socket per minute.
const _: () = assert!(
    DEPTH200_SWAP_COMMAND_CHANNEL_DEPTH < DEPTH_SWAP_COMMAND_CHANNEL_DEPTH,
    "depth-200 swaps one instrument per socket per minute; a queue as deep as \
     depth-20's only delays its wedge signal"
);

/// The I-P1-11 composite identity, as the depth pools key on it.
type Key = (u64, u8);

#[must_use]
fn key_of(instrument: SubscribeInstrument) -> Key {
    (instrument.security_id, instrument.segment.binary_code())
}

/// The published depth-20 ranking: the gainer-eligible top 250 stock-option
/// contracts by lots in the window, in rank order.
///
/// Same single-writer / single-reader shape as
/// [`crate::depth200_candidates::Depth200Candidates`], and the same rows —
/// a [`Depth200Candidate`] is identity plus the number the decision was made
/// on, which is all either pool needs. A second row type would be a second
/// thing to keep in step with the first.
#[derive(Debug, Default)]
pub struct Depth20Candidates {
    ranked: ArcSwapOption<Vec<Depth200Candidate>>,
}

/// Gauge: how many contracts the last depth-20 ranking held, or
/// [`NOT_YET_RANKED`] before the first.
pub const DEPTH20_RANKED_CANDIDATES_GAUGE: &str = "tv_depth20_ranked_candidates";

impl Depth20Candidates {
    /// A view that has never been published to, with its gauge pre-registered
    /// at [`NOT_YET_RANKED`] so a never-created series cannot read as a dead
    /// process.
    #[must_use]
    pub fn new() -> Self {
        metrics::gauge!(DEPTH20_RANKED_CANDIDATES_GAUGE).set(NOT_YET_RANKED);
        Self {
            ranked: ArcSwapOption::empty(),
        }
    }

    /// Replaces the published ranking. Takes the already-ranked,
    /// already-gainer-filtered rows; it does NOT rank.
    pub fn publish(&self, candidates: Vec<Depth200Candidate>) {
        metrics::gauge!(DEPTH20_RANKED_CANDIDATES_GAUGE).set(candidates.len() as f64);
        self.ranked.store(Some(Arc::new(candidates)));
    }

    /// The last published ranking, or `None` if nothing has been published.
    /// `None` and `Some(empty)` are DIFFERENT answers: the caller falls back
    /// to the legacy layout on `None` and holds on `Some(empty)`.
    #[must_use]
    pub fn latest(&self) -> Option<Arc<Vec<Depth200Candidate>>> {
        self.ranked.load_full()
    }
}

/// The process-wide published depth-20 ranking. Global for the reason
/// [`crate::depth200_candidates::global_depth200_candidates`] is.
#[must_use]
pub fn global_depth20_candidates() -> &'static Arc<Depth20Candidates> {
    static VIEW: std::sync::OnceLock<Arc<Depth20Candidates>> = std::sync::OnceLock::new();
    VIEW.get_or_init(|| Arc::new(Depth20Candidates::new()))
}

/// What one minute of ranked depth-20 steering decided.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Depth20RankedDecision {
    /// The swaps to send, per socket, in the shape `apply_depth20_plan` takes.
    pub plan: Depth20Plan,
    /// Held instruments that are on the ranking and were therefore left alone.
    pub kept: usize,
    /// Departures refused by the per-socket cap this minute. Retried next
    /// minute — the ranking is recomputed, so they may no longer be departures.
    pub capped: usize,
    /// Ranked contracts with no departure anywhere to fund them.
    pub unplaced: usize,
    /// Departures with no ranked arrival left to take their slot. The socket
    /// keeps them: a socket is never emptied or shrunk.
    pub unfunded_departures: usize,
}

impl Depth20RankedDecision {
    /// Whether nothing needs to go on the wire.
    #[must_use]
    pub fn is_quiet(&self) -> bool {
        self.plan.is_quiet()
    }
}

/// Plans one minute of depth-20 steering from the published ranking.
///
/// `held[i]` is what wire socket `i` currently holds (its `SubscribeGuard`
/// view, as `depth_rebalance` keeps it). `ranked` is the published top-250 in
/// rank order. Returns swaps only — never an add, never a removal — so the
/// pool's instrument count is invariant across the minute.
///
/// An EMPTY ranking (`Some(empty)` at the caller) plans nothing: the ranking
/// layer ran and selected nothing (pre-open, a down day under the gainer
/// filter, or every row refused by the monotonicity gate), and the honest
/// answer is to hold, not to fall back to the banned layout.
#[must_use]
pub fn plan_depth20_ranked_minute(
    held: &[Vec<SubscribeInstrument>],
    ranked: &[Depth200Candidate],
) -> Depth20RankedDecision {
    let mut decision = Depth20RankedDecision::default();
    if ranked.is_empty() {
        decision.plan.sockets_left_alone = held.len();
        return decision;
    }

    // The desired set, and everything the pool holds across every socket. A
    // contract held on two sockets at once (a reconnect replay racing a swap)
    // is one held key; the diff below treats the second copy as neither an
    // arrival nor a departure, which is the conservative reading.
    let desired: BTreeSet<Key> = ranked.iter().map(Depth200Candidate::key).collect();
    let held_anywhere: BTreeSet<Key> = held.iter().flatten().copied().map(key_of).collect();

    // Arrivals in RANK order — the best unheld contract is placed first — and
    // each taken at most once across every socket (`claimed`), so two sockets
    // funding a departure in the same minute cannot both subscribe it. Dhan
    // answers a duplicate subscription with an 804, which is Fatal.
    // Arrivals come from the ENTRY ranks only; `desired` above spans the whole
    // published list (the exit band), which is what makes a held contract at
    // rank 260 "kept" rather than swapped for rank 250.
    let mut arrivals = ranked
        .iter()
        .take(DEPTH20_ENTRY_RANKS)
        .filter(|c| !held_anywhere.contains(&c.key()))
        .map(|c| SubscribeInstrument {
            security_id: c.security_id,
            segment: c.segment,
        })
        .peekable();

    for (socket, held_here) in held.iter().enumerate() {
        if held_here.is_empty() {
            // Nothing to release, so nothing can arrive. Counted, never filled:
            // a first subscription is a different command shape.
            decision.plan.sockets_left_alone += 1;
            continue;
        }
        let mut socket_plan = Depth20SocketPlan {
            socket,
            ..Depth20SocketPlan::default()
        };
        let mut seen_here: BTreeSet<Key> = BTreeSet::new();
        for release in held_here.iter().copied() {
            let key = key_of(release);
            if !seen_here.insert(key) {
                // The same instrument twice on one socket: one release at most.
                continue;
            }
            if desired.contains(&key) {
                decision.kept += 1;
                continue;
            }
            // PEEK, then cap, then consume — the depth-200 planner's ordering
            // (`depth200_ranked_steer::plan_ranked_minute`), and load-bearing
            // for the same reason.
            //
            // `capped` and `unfunded_departures` exist to separate "not
            // allowed to give it" from "nothing to give", because they have
            // OPPOSITE remedies: `capped` says raise the per-socket cap or
            // widen the band, `unfunded_departures` says the ranking is
            // shorter than the pool and no cap change would help. Testing the
            // cap FIRST (the shape here until 2026-09-13) reported `capped`
            // for a departure that arrived at a drained queue, so a socket at
            // its budget with nothing left to place read as cap pressure and
            // the documented remedy would have changed nothing at all.
            //
            // `continue`, never `break`: later releases on this socket may be
            // on the ranking and belong in `kept`.
            if arrivals.peek().is_none() {
                // Fewer ranked contracts than departures: keep the slot
                // occupied rather than shrink the socket.
                socket_plan.unused_departures += 1;
                decision.unfunded_departures += 1;
                continue;
            }
            if socket_plan.swaps.len() >= MAX_RANKED_DEPTH20_SWAPS_PER_SOCKET_PER_MINUTE {
                // Refused by the cap with a candidate still waiting. Leaving
                // it in the iterator is what lets the terminal
                // `arrivals.count()` report it as `unplaced` rather than
                // deleting it from the plan and from every counter.
                decision.capped += 1;
                continue;
            }
            let Some(take) = arrivals.next() else {
                // Unreachable: the peek above returned `Some` and nothing
                // between there and here advances the iterator. A `continue`
                // rather than an `expect` because `panic = "abort"` on the
                // release profile turns a wrong assumption here into process
                // death mid-session, and `clippy::expect_used` is denied
                // outside tests.
                continue;
            };
            socket_plan.swaps.push((release, take));
        }
        if !socket_plan.swaps.is_empty() {
            decision.plan.sockets.push(socket_plan);
        }
    }
    decision.unplaced = arrivals.count();
    decision
}

/// Counter: ranked depth-20 swaps by outcome.
pub const DEPTH20_RANKED_SWAPS_COUNTER: &str = "tv_depth20_ranked_swaps_total";
/// Gauge: held instruments the last minute's ranking agreed with.
pub const DEPTH20_RANKED_KEPT_GAUGE: &str = "tv_depth20_ranked_kept";
/// Every `outcome` label the counter carries. `planned` is a swap handed to
/// the wire layer; the other three are refusals the ranking could not place.
pub const DEPTH20_RANKED_OUTCOME_LABELS: [&str; 4] =
    ["planned", "capped", "unplaced", "unfunded_departure"];

/// Records one minute's decision. Absolute-free: every value is this minute's
/// count, and a quiet minute records zeros so the series stays dense.
pub fn record_depth20_ranked_decision(decision: &Depth20RankedDecision) {
    metrics::gauge!(DEPTH20_RANKED_KEPT_GAUGE).set(decision.kept as f64);
    let planned = decision.plan.swap_count() as u64;
    metrics::counter!(DEPTH20_RANKED_SWAPS_COUNTER, "outcome" => "planned").increment(planned);
    metrics::counter!(DEPTH20_RANKED_SWAPS_COUNTER, "outcome" => "capped")
        .increment(decision.capped as u64);
    metrics::counter!(DEPTH20_RANKED_SWAPS_COUNTER, "outcome" => "unplaced")
        .increment(decision.unplaced as u64);
    metrics::counter!(DEPTH20_RANKED_SWAPS_COUNTER, "outcome" => "unfunded_departure")
        .increment(decision.unfunded_departures as u64);
}

/// Registers every series at zero (or [`NOT_YET_RANKED`]) before the first
/// minute, so a quiet session and a dead exporter do not look identical.
pub fn pre_register_depth20_ranked_counters() {
    metrics::gauge!(DEPTH20_RANKED_KEPT_GAUGE).set(0.0);
    for outcome in DEPTH20_RANKED_OUTCOME_LABELS {
        metrics::counter!(DEPTH20_RANKED_SWAPS_COUNTER, "outcome" => outcome).increment(0);
    }
    // Touching the global constructs it, which registers the candidates gauge
    // at NOT_YET_RANKED; the view itself is not needed here.
    let _view = global_depth20_candidates();
}

#[cfg(test)]
mod tests {
    use super::*;
    use tickvault_common::types::ExchangeSegment;

    const FNO: ExchangeSegment = ExchangeSegment::NseFno;

    #[test]
    fn pre_register_depth20_ranked_counters_covers_every_label() {
        pre_register_depth20_ranked_counters();
        record_depth20_ranked_decision(&Depth20RankedDecision {
            kept: 2,
            capped: 1,
            unplaced: 1,
            unfunded_departures: 1,
            ..Depth20RankedDecision::default()
        });
        assert_eq!(DEPTH20_RANKED_OUTCOME_LABELS.len(), 4);
    }
    const IDX: ExchangeSegment = ExchangeSegment::IdxI;

    fn candidate(id: u64, underlying: u64, lots: u64) -> Depth200Candidate {
        Depth200Candidate {
            security_id: id,
            segment: FNO,
            underlying_id: underlying,
            window_lots_milli: lots,
        }
    }

    fn ins(id: u64, segment: ExchangeSegment) -> SubscribeInstrument {
        SubscribeInstrument {
            security_id: id,
            segment,
        }
    }

    fn held_fno(ids: &[u64]) -> Vec<SubscribeInstrument> {
        ids.iter().map(|&id| ins(id, FNO)).collect()
    }

    #[test]
    fn plan_depth20_ranked_minute_is_quiet_when_every_socket_is_on_the_ranking() {
        let held = vec![held_fno(&[1, 2]), held_fno(&[3])];
        let ranked = [
            candidate(3, 30, 9),
            candidate(1, 10, 8),
            candidate(2, 20, 7),
        ];
        let d = plan_depth20_ranked_minute(&held, &ranked);
        assert!(d.is_quiet());
        assert_eq!(d.kept, 3);
        assert_eq!((d.capped, d.unplaced, d.unfunded_departures), (0, 0, 0));
    }

    #[test]
    fn plan_depth20_ranked_minute_swaps_off_ranking_holdings_onto_the_best_unheld_first() {
        // Socket 0 holds an index option (the banned class) and one ranked
        // contract; socket 1 holds two off-ranking contracts. Three ranked
        // contracts are unheld; the best (highest lots) is placed first.
        let held = vec![vec![ins(9001, IDX), ins(1, FNO)], held_fno(&[701, 702])];
        let ranked = [
            candidate(1, 10, 900),
            candidate(5, 50, 500),
            candidate(4, 40, 400),
            candidate(3, 30, 300),
        ];
        let d = plan_depth20_ranked_minute(&held, &ranked);
        assert_eq!(d.kept, 1);
        assert_eq!(d.plan.swap_count(), 3);
        assert_eq!(d.plan.sockets[0].socket, 0);
        assert_eq!(d.plan.sockets[0].swaps, vec![(ins(9001, IDX), ins(5, FNO))]);
        assert_eq!(d.plan.sockets[1].socket, 1);
        assert_eq!(
            d.plan.sockets[1].swaps,
            vec![(ins(701, FNO), ins(4, FNO)), (ins(702, FNO), ins(3, FNO))]
        );
        assert_eq!((d.capped, d.unplaced, d.unfunded_departures), (0, 0, 0));
    }

    #[test]
    fn plan_depth20_ranked_minute_caps_each_socket_at_the_channel_depth() {
        let cap = MAX_RANKED_DEPTH20_SWAPS_PER_SOCKET_PER_MINUTE;
        let departures: Vec<u64> = (700..700 + cap as u64 + 3).collect();
        let held = vec![held_fno(&departures)];
        let ranked: Vec<Depth200Candidate> = (1..=(cap as u64 + 3))
            .map(|i| candidate(i, i * 10, 1000 - i))
            .collect();
        let d = plan_depth20_ranked_minute(&held, &ranked);
        assert_eq!(d.plan.swap_count(), cap);
        assert_eq!(d.capped, 3);
        // The three arrivals the cap refused this minute are counted as
        // unplaced, not silently dropped.
        assert_eq!(d.unplaced, 3);
    }

    /// An exhausted arrival queue is NOT a cap refusal, even on a socket that
    /// has already spent its whole per-minute budget.
    ///
    /// The two counters exist to separate "not allowed to give it" from
    /// "nothing to give": `capped` says raise the cap or widen the band,
    /// `unfunded_departures` says the ranking is shorter than the pool. With
    /// the cap tested BEFORE the queue (the shape here until 2026-09-13) the
    /// last departure below reported `capped`, so the documented remedy —
    /// raising the cap — would have changed nothing at all.
    #[test]
    fn an_exhausted_arrival_queue_is_unfunded_not_capped() {
        let cap = MAX_RANKED_DEPTH20_SWAPS_PER_SOCKET_PER_MINUTE;
        // Exactly `cap` arrivals, `cap + 1` departures: the first `cap`
        // departures drain the queue and fill the budget, so the last one hits
        // an empty queue AND a full budget at the same instant.
        let departures: Vec<u64> = (700..700 + cap as u64 + 1).collect();
        let held = vec![held_fno(&departures)];
        let ranked: Vec<Depth200Candidate> = (1..=cap as u64)
            .map(|i| candidate(i, i * 10, 1000 - i))
            .collect();
        let d = plan_depth20_ranked_minute(&held, &ranked);
        assert_eq!(d.plan.swap_count(), cap);
        assert_eq!(
            d.capped, 0,
            "the last departure was refused by an empty ranking, not by the cap"
        );
        assert_eq!(d.unfunded_departures, 1);
        assert_eq!(d.plan.sockets[0].unused_departures, 1);
        assert_eq!(d.unplaced, 0, "every arrival found a home");
    }

    #[test]
    fn plan_depth20_ranked_minute_never_takes_one_arrival_on_two_sockets() {
        let held = vec![held_fno(&[701]), held_fno(&[702])];
        let ranked = [candidate(1, 10, 100)];
        let d = plan_depth20_ranked_minute(&held, &ranked);
        assert_eq!(d.plan.swap_count(), 1);
        assert_eq!(d.plan.sockets[0].swaps, vec![(ins(701, FNO), ins(1, FNO))]);
        // Socket 1's departure had nothing left to fund it and keeps its slot.
        assert_eq!(d.unfunded_departures, 1);
        assert!(d.plan.sockets.iter().all(|s| s.socket != 1));
    }

    #[test]
    fn plan_depth20_ranked_minute_holds_on_an_empty_ranking_and_leaves_empty_sockets_alone() {
        let held = vec![held_fno(&[701]), Vec::new()];
        let empty = plan_depth20_ranked_minute(&held, &[]);
        assert!(empty.is_quiet());
        assert_eq!(empty.plan.sockets_left_alone, 2);

        let d = plan_depth20_ranked_minute(&held, &[candidate(1, 10, 5), candidate(2, 20, 4)]);
        assert_eq!(d.plan.sockets_left_alone, 1);
        assert_eq!(d.plan.swap_count(), 1);
        assert_eq!(d.unplaced, 1);
    }

    #[test]
    fn plan_depth20_ranked_minute_keys_on_the_composite_so_a_same_id_index_option_is_off_ranking() {
        // Same numeric id, different segment: the held index option is NOT
        // the ranked stock option and must be swapped out.
        let held = vec![vec![ins(1, IDX)]];
        let ranked = [candidate(1, 10, 100)];
        let d = plan_depth20_ranked_minute(&held, &ranked);
        assert_eq!(d.kept, 0);
        assert_eq!(d.plan.sockets[0].swaps, vec![(ins(1, IDX), ins(1, FNO))]);
    }

    #[test]
    fn plan_depth20_ranked_minute_releases_a_duplicate_holding_once() {
        let held = vec![held_fno(&[701, 701])];
        let ranked = [candidate(1, 10, 100), candidate(2, 20, 90)];
        let d = plan_depth20_ranked_minute(&held, &ranked);
        assert_eq!(d.plan.swap_count(), 1);
        assert_eq!(d.unplaced, 1);
    }

    #[test]
    fn publish_then_latest_round_trips_and_latest_is_none_before_the_first_publish() {
        let view = Depth20Candidates::new();
        assert!(view.latest().is_none());
        view.publish(vec![candidate(1, 10, 5)]);
        assert_eq!(view.latest().map(|v| v.len()), Some(1));
        view.publish(Vec::new());
        assert_eq!(view.latest().map(|v| v.len()), Some(0));
    }

    #[test]
    fn global_depth20_candidates_hands_out_one_view() {
        let a = global_depth20_candidates();
        let b = global_depth20_candidates();
        assert!(Arc::ptr_eq(a, b));
    }

    #[test]
    fn record_depth20_ranked_decision_and_pre_register_depth20_ranked_counters_cover_every_label() {
        pre_register_depth20_ranked_counters();
        let held = vec![held_fno(&[701])];
        let d = plan_depth20_ranked_minute(&held, &[candidate(1, 10, 5), candidate(2, 20, 4)]);
        record_depth20_ranked_decision(&d);
        record_depth20_ranked_decision(&Depth20RankedDecision::default());
        let mut labels: Vec<&str> = DEPTH20_RANKED_OUTCOME_LABELS.to_vec();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), DEPTH20_RANKED_OUTCOME_LABELS.len());
        assert_eq!(d.plan.swap_count() + d.unplaced, 2);
    }

    /// The hysteresis band: a held contract inside the exit set but OUTSIDE
    /// the entry cut is kept, one past the exit set departs, and arrivals are
    /// only ever taken from the entry cut. Without the band, rank 251 in one
    /// 5-second window and rank 249 in the next is a swap each way.
    #[test]
    fn plan_depth20_ranked_minute_keeps_a_held_contract_inside_the_exit_band() {
        let ranked: Vec<Depth200Candidate> = (1..=DEPTH20_EXIT_RANKS as u64)
            .map(|i| candidate(i, i, 10_000 - i))
            .collect();
        let inside_band = DEPTH20_ENTRY_RANKS as u64 + 10; // rank 260
        let past_band = DEPTH20_EXIT_RANKS as u64 + 1; // rank 301, unranked
        let held = vec![vec![ins(inside_band, FNO), ins(past_band, FNO)]];
        let decision = plan_depth20_ranked_minute(&held, &ranked);
        assert_eq!(decision.kept, 1, "rank 260 is inside the exit band: kept");
        let swaps = &decision.plan.sockets[0].swaps;
        assert_eq!(swaps.len(), 1, "only the contract past the band departs");
        assert_eq!(swaps[0].0.security_id, past_band);
        assert!(
            swaps[0].1.security_id <= DEPTH20_ENTRY_RANKS as u64,
            "an arrival comes from the entry cut, never from the band"
        );
    }

    /// A socket holding rank 300 exactly (the last exit rank) is kept; a
    /// ranking shorter than the entry cut still only fills from what exists.
    #[test]
    fn plan_depth20_ranked_minute_band_edges_are_inclusive_at_exit_and_bounded_at_entry() {
        let ranked: Vec<Depth200Candidate> = (1..=DEPTH20_EXIT_RANKS as u64)
            .map(|i| candidate(i, i, 10_000 - i))
            .collect();
        let held = vec![vec![ins(DEPTH20_EXIT_RANKS as u64, FNO)]];
        let decision = plan_depth20_ranked_minute(&held, &ranked);
        assert_eq!(decision.kept, 1);
        assert!(decision.plan.is_quiet());
        // Arrivals: with 300 ranked and one socket of one unranked contract,
        // the arrival is rank 1 — from the entry cut.
        let held = vec![vec![ins(9_999, FNO)]];
        let decision = plan_depth20_ranked_minute(&held, &ranked);
        assert_eq!(decision.plan.sockets[0].swaps[0].1.security_id, 1);
    }
}
