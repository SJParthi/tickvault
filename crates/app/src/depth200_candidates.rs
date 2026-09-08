//! What depth-200 SHOULD be holding, published from the drain and read by the
//! per-minute steering loop.
//!
//! # Why this exists (2026-09-08)
//!
//! The operator's 2026-09-06 lock
//! (`websocket-connection-scope-lock.md`, "DEPTH IS STOCK OPTIONS ONLY, RANKED
//! BY VOLUME") puts the top five STOCK-option contracts by traded volume on the
//! five depth-200 sockets, each a DISTINCT underlying. The selector that
//! actually dials them ranks on `close_pct_from_prev_day` of the underlying
//! SPOT instead, so four of the five sockets carry NIFTY/BANKNIFTY INDEX
//! options — the exact class the lock bans.
//!
//! [`crate::volume_leaderboard::VolumeLeaderboard::rank_distinct_underlying`]
//! computes the right answer and has zero production callers, because it needs
//! `&mut self` on the ingest that lives on the frame-drain task while
//! `run_depth_rebalance` is a separate `tokio::spawn` with no handle to it. A
//! `Mutex` on the drain's state is not available: that is the per-packet hot
//! path.
//!
//! So this module is the seam. The drain PUBLISHES the ranking it already
//! computes; the steering loop READS it. Same shape, same reasoning and the
//! same two-slot ownership argument as [`crate::depth_subscription_view`],
//! which carries the answer in the opposite direction.
//!
//! # This change does NOT move a socket
//!
//! Deliberately. It publishes the ranking and REPORTS how far the live pool has
//! drifted from it — nothing subscribes, nothing unsubscribes. A mandate
//! violation that is measured is one an operator can act on; acting on it in
//! the same change would move five deep sockets onto books whose depth has
//! never been measured, on a lock whose own text records 800 rows/minute
//! against 100,800 when thin contracts took those sockets on 2026-08-26.
//!
//! # "Not yet ranked" is NOT "ranked, and empty"
//!
//! The distinction is load-bearing and is carried in the TYPE rather than by a
//! sentinel value. [`ArcSwapOption`] holds `None` until the first publish; a
//! ranking that legitimately found nothing publishes `Some(vec![])`.
//!
//! Collapsing the two would make the very first minute of every session report
//! "all 5 depth-200 sockets are off the ranking" — before a single tick has
//! been ranked, and while the pre-open holds no volume for anything. That is a
//! false alarm that arrives once per session, i.e. exactly often enough to
//! train an operator to ignore the signal.
//!
//! The gauges carry the same distinction, in band: `-1.0` means NOT YET RANKED
//! and is impossible for a length or a count, so it can never be confused with
//! a real reading of zero.
//!
//! # Complexity
//!
//! * publish — O(k) in the published list (k = 5), on the drain's 5-second
//!   timer arm. **Never per packet.** One `Vec` of 5 and one `Arc`, on a path
//!   that already allocates a 250-row ranking beside it.
//! * read — O(1): one lock-free `ArcSwap` load plus an `Arc` clone.
//! * divergence — O(held × k) = O(5 × 5), once per minute, on the steering
//!   loop's own task.
//!
//! # Cost
//!
//! Two gauges, neither EMF-selected and therefore neither shipped to
//! CloudWatch. That is a deliberate choice, not an oversight: the September
//! forecast read $142.24 against an automatic `STOP_EC2_INSTANCES` line at
//! $135.00, so an EMF name (~$0.30/mo) needs an operator lever rather than a
//! cost note. Both are on the local `/metrics` exporter, which is where this
//! measurement is read from until that decision is made.

use std::sync::Arc;

use arc_swap::ArcSwapOption;
use tickvault_common::types::ExchangeSegment;

use crate::volume_leaderboard::RankedContract;

/// Gauge: how many contracts the last published ranking held.
///
/// `-1.0` until the first publish — see the module header. NOT EMF-selected.
pub const RANKED_CANDIDATES_GAUGE: &str = "tv_depth200_ranked_candidates";

/// Gauge: how many live depth-200 sockets hold a contract that is NOT in the
/// published ranking.
///
/// `-1.0` until the first publish. NOT EMF-selected.
pub const SOCKETS_OFF_RANKING_GAUGE: &str = "tv_depth200_sockets_off_ranking";

/// How many contracts the ranking publishes: one per depth-200 socket.
///
/// Derived from the pool's own layout rather than written as `5`, so a change
/// to the socket budget cannot leave this list sized for the old one. The pool
/// is [`crate::depth200_atm::DEPTH_200_ATM_SOCKETS`] at-the-money sockets
/// (NIFTY and BANKNIFTY, CE and PE) plus the single top-mover socket at index
/// [`crate::depth200_atm::DEPTH_200_TOP_MOVER_SOCKET`].
///
/// The lock says those five must be STOCK options with distinct underlyings;
/// this constant is only the COUNT, and the const-assert below pins that the
/// two ways of expressing the layout agree.
pub const DEPTH_200_SOCKET_BUDGET: usize = crate::depth200_atm::DEPTH_200_ATM_SOCKETS + 1;

const _: () = assert!(
    DEPTH_200_SOCKET_BUDGET == crate::depth200_atm::DEPTH_200_TOP_MOVER_SOCKET + 1,
    "the top-mover socket must be the LAST of the depth-200 sockets; if these \
     two disagree, the published ranking is sized for a pool that does not exist"
);

/// How many DISTINCT-underlying contracts the ranking publishes: the five
/// ENTRY slots plus a hysteresis band of three.
///
/// The 2026-09-07 lock names a hysteresis band as the remedy for a churning
/// per-window board, and until 2026-09-08 (SECOND) depth-200 had none: the
/// published list was exactly five, so a held contract that slipped to sixth
/// for ONE five-second window was swapped out and, a window later, swapped
/// back. Each of those is two wire calls on a deep socket whose book takes
/// seconds to refill.
///
/// The rule is the depth-20 one, scaled to five sockets: a contract ENTERS
/// only from the first [`DEPTH_200_SOCKET_BUDGET`] ranks; a contract already
/// held is KEPT while it stays anywhere inside this longer list. Three ranks
/// of band, not one, because the distinct-underlying pass makes rank 6 a
/// different STOCK from rank 5, and a stock that is the sixth-busiest one
/// window and the fifth the next is precisely the churn a band exists for.
///
/// Derived from the budget rather than written as `8`, so a change to the
/// socket count moves the band with it.
pub const DEPTH200_EXIT_UNDERLYINGS: usize = DEPTH_200_SOCKET_BUDGET + DEPTH200_HYSTERESIS_RANKS;

/// The band width — see [`DEPTH200_EXIT_UNDERLYINGS`].
pub const DEPTH200_HYSTERESIS_RANKS: usize = 3;

const _: () = assert!(
    DEPTH200_EXIT_UNDERLYINGS > DEPTH_200_SOCKET_BUDGET,
    "a band no wider than the entry set is not a band"
);

/// The ENTRY set of a published ranking: the contracts that may be swapped
/// IN. The remainder of the list is the hysteresis band, which only decides
/// what is KEPT.
#[must_use]
pub fn entry_set(ranked: &[Depth200Candidate]) -> &[Depth200Candidate] {
    &ranked[..ranked.len().min(DEPTH_200_SOCKET_BUDGET)]
}

/// The in-band "no ranking has been published yet" value for both gauges.
///
/// Negative because a length and a count are both non-negative, so this cannot
/// collide with a real reading. A separate boolean gauge would be a second
/// series to keep in step with the first, and the two could disagree.
pub const NOT_YET_RANKED: f64 = -1.0;

/// One contract the ranking says a depth-200 socket should carry.
///
/// A narrowed copy of [`RankedContract`] rather than the type itself: the
/// steering loop needs identity plus the number the decision was made on, and
/// publishing the full row would carry `volume` — the raw cumulative
/// observation — into a consumer that must never rank on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Depth200Candidate {
    /// The CONTRACT's own Dhan `SecurityId`, never the underlying's.
    pub security_id: u64,
    /// The contract's segment. Half of the I-P1-11 identity, never dropped —
    /// Dhan reuses one numeric id across segments.
    pub segment: ExchangeSegment,
    /// The underlying's `SecurityId`. Carried so a reader can see WHY two
    /// contracts could not both be picked.
    pub underlying_id: u64,
    /// The rank key: lots traded in the window that just closed, × 1000.
    pub window_lots_milli: u64,
}

impl Depth200Candidate {
    /// The I-P1-11 composite identity, as the wire-byte pair the depth pools
    /// key on.
    #[must_use]
    pub fn key(&self) -> (u64, u8) {
        (self.security_id, self.segment.binary_code())
    }
}

/// The published depth-200 ranking.
///
/// One writer (the frame drain's 5-second timer arm) and one reader (the
/// per-minute steering loop), so the single slot has no clobbering hazard —
/// unlike [`crate::depth_subscription_view`], which needs two slots precisely
/// because it has two independent publishers.
#[derive(Debug, Default)]
pub struct Depth200Candidates {
    ranked: ArcSwapOption<Vec<Depth200Candidate>>,
}

impl Depth200Candidates {
    /// A view that has never been published to.
    ///
    /// Pre-registers both gauges at [`NOT_YET_RANKED`]. Registered HERE rather
    /// than on first publish because a never-created series reads as missing
    /// data, which is visually identical to a dead process — the state this
    /// measurement exists to tell apart from a healthy one.
    #[must_use]
    pub fn new() -> Self {
        metrics::gauge!(RANKED_CANDIDATES_GAUGE).set(NOT_YET_RANKED);
        metrics::gauge!(SOCKETS_OFF_RANKING_GAUGE).set(NOT_YET_RANKED);
        Self {
            ranked: ArcSwapOption::empty(),
        }
    }

    /// Replaces the published ranking.
    ///
    /// Takes the already-ranked, already-distinct-underlying rows. It does NOT
    /// rank: the ranking rolls per-cadence baselines forward inside the
    /// leaderboard, so a second pass over the same window would measure a
    /// window that has already been consumed. See
    /// [`crate::volume_leaderboard::distinct_underlying_over`].
    pub fn publish(&self, candidates: Vec<Depth200Candidate>) {
        metrics::gauge!(RANKED_CANDIDATES_GAUGE).set(candidates.len() as f64);
        self.ranked.store(Some(Arc::new(candidates)));
    }

    /// The last published ranking, or `None` if nothing has been published.
    ///
    /// `None` and `Some(empty)` are DIFFERENT answers and callers must treat
    /// them differently — see the module header.
    #[must_use]
    pub fn latest(&self) -> Option<Arc<Vec<Depth200Candidate>>> {
        self.ranked.load_full()
    }

    /// How many contracts the last publish held, or `None` before the first.
    #[must_use]
    pub fn published_len(&self) -> Option<usize> {
        self.latest().map(|v| v.len())
    }
}

/// Turns ranked rows into the narrowed candidates this module publishes.
///
/// Free function rather than a `From` impl so the narrowing is a named,
/// greppable step: the field that is DROPPED (`volume`, the raw cumulative
/// observation) is the one a future reader must not start ranking on.
#[must_use]
pub fn candidates_from_ranked(ranked: &[RankedContract]) -> Vec<Depth200Candidate> {
    ranked
        .iter()
        .map(|r| Depth200Candidate {
            security_id: r.security_id,
            segment: r.segment,
            underlying_id: r.underlying_id,
            window_lots_milli: r.window_lots_milli,
        })
        .collect()
}

/// What the live depth-200 pool holds versus what the ranking says it should.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Divergence {
    /// Held instruments that are NOT in the published ranking, as `(id,
    /// segment)`.
    pub held_off_ranking: Vec<(u64, ExchangeSegment)>,
    /// Ranked contracts that hold no depth-200 socket.
    pub ranked_unheld: Vec<(u64, ExchangeSegment)>,
    /// How many sockets were reconciled as holding something at all.
    pub held_total: usize,
}

impl Divergence {
    /// Whether the live pool already matches the ranking.
    #[must_use]
    pub fn is_aligned(&self) -> bool {
        self.held_off_ranking.is_empty() && self.ranked_unheld.is_empty()
    }
}

/// Compares what the pool holds against the published ranking.
///
/// Pure, so the interesting cases — an empty ranking, an empty pool, a
/// same-id-different-segment near miss — are unit tests rather than a live
/// surprise. O(held × ranked) with both bounded by 5.
///
/// The comparison is on the I-P1-11 COMPOSITE. Keying on the bare id would
/// report a held index option as "on the ranking" because a stock option
/// happens to share its number, which is the failure this whole change exists
/// to make visible.
///
/// Since the hysteresis band (2026-09-08 SECOND) the two halves read
/// DIFFERENT parts of the list, on purpose: `held_off_ranking` is judged
/// against the WHOLE published list (a held contract inside the band is not
/// off the ranking — the planner keeps it), while `ranked_unheld` is judged
/// against the [`entry_set`] only (a band contract that holds no socket is not
/// a contract the planner would ever place, so reporting it as "should hold"
/// would be a divergence nothing can close).
#[must_use]
pub fn diverge(held: &[(u64, ExchangeSegment)], ranked: &[Depth200Candidate]) -> Divergence {
    let ranked_keys: Vec<(u64, u8)> = ranked.iter().map(Depth200Candidate::key).collect();
    let held_keys: Vec<(u64, u8)> = held
        .iter()
        .map(|(id, seg)| (*id, seg.binary_code()))
        .collect();

    let held_off_ranking = held
        .iter()
        .zip(held_keys.iter())
        .filter(|(_, key)| !ranked_keys.contains(key))
        .map(|((id, seg), _)| (*id, *seg))
        .collect();

    let ranked_unheld = entry_set(ranked)
        .iter()
        .filter(|c| !held_keys.contains(&c.key()))
        .map(|c| (c.security_id, c.segment))
        .collect();

    Divergence {
        held_off_ranking,
        ranked_unheld,
        held_total: held.len(),
    }
}

/// The process-wide published ranking.
///
/// Global for the same reason [`crate::depth_subscription_view::global_depth_subscription_view`]
/// is: the writer and the reader sit on opposite sides of a `tokio::spawn`
/// whose signature already carries nine arguments, and threading a tenth
/// through it would put the wiring's cost in the place least related to it.
///
/// The single-writer argument is unaffected by being global — the accessor
/// hands out a shared reference, and `publish` is called from exactly one arm
/// of one `select!`.
#[must_use]
pub fn global_depth200_candidates() -> &'static Arc<Depth200Candidates> {
    static VIEW: std::sync::OnceLock<Arc<Depth200Candidates>> = std::sync::OnceLock::new();
    VIEW.get_or_init(|| Arc::new(Depth200Candidates::new()))
}

/// Publishes the divergence gauge and returns what was compared.
///
/// `None` when nothing has been ranked yet: the gauge is set to
/// [`NOT_YET_RANKED`] and no divergence is claimed. That is the whole reason
/// the "not yet ranked" state is carried separately — reporting five diverging
/// sockets before the first ranking exists would be a false alarm on every
/// session's first minute.
pub fn report_divergence(
    candidates: &Depth200Candidates,
    held: &[(u64, ExchangeSegment)],
) -> Option<Divergence> {
    let Some(ranked) = candidates.latest() else {
        metrics::gauge!(SOCKETS_OFF_RANKING_GAUGE).set(NOT_YET_RANKED);
        return None;
    };
    let divergence = diverge(held, &ranked);
    metrics::gauge!(SOCKETS_OFF_RANKING_GAUGE).set(divergence.held_off_ranking.len() as f64);
    Some(divergence)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FNO: ExchangeSegment = ExchangeSegment::NseFno;
    const IDX: ExchangeSegment = ExchangeSegment::IdxI;

    fn candidate(security_id: u64, underlying_id: u64, lots: u64) -> Depth200Candidate {
        Depth200Candidate {
            security_id,
            segment: FNO,
            underlying_id,
            window_lots_milli: lots,
        }
    }

    fn ranked_row(security_id: u64, underlying_id: u64, lots: u64) -> RankedContract {
        RankedContract {
            security_id,
            segment: FNO,
            underlying_id,
            volume: 12_345,
            window_lots_milli: lots,
        }
    }

    #[test]
    fn published_len_is_not_yet_ranked_rather_than_empty_on_a_fresh_view() {
        // The distinction the whole module turns on. `None` here is what stops
        // the first minute of a session reporting five diverging sockets before
        // a single tick has been ranked.
        let view = Depth200Candidates::new();
        assert!(view.latest().is_none());
        assert_eq!(view.published_len(), None);
    }

    #[test]
    fn publishing_an_empty_ranking_is_a_ranking_not_an_absence() {
        // A ranking that legitimately found nothing is a REAL answer and must
        // be distinguishable from never having ranked.
        let view = Depth200Candidates::new();
        view.publish(Vec::new());
        assert!(
            view.latest().is_some(),
            "an empty publish must still register as published"
        );
        assert_eq!(view.published_len(), Some(0));
    }

    #[test]
    fn publish_replaces_rather_than_accumulates() {
        // The ranking is recomputed from scratch every 5 seconds. Merging would
        // keep a contract that left the board alive forever, which is the
        // overstating direction — a socket would read correct after the reason
        // it was correct had gone.
        let view = Depth200Candidates::new();
        view.publish(vec![candidate(1, 100, 9), candidate(2, 200, 8)]);
        view.publish(vec![candidate(3, 300, 7)]);
        let latest = view.latest().expect("published");
        assert_eq!(latest.len(), 1);
        assert_eq!(latest[0].security_id, 3);
    }

    #[test]
    fn candidates_from_ranked_keeps_identity_and_the_rank_key() {
        let rows = [ranked_row(11, 100, 5_000), ranked_row(22, 200, 4_000)];
        let out = candidates_from_ranked(&rows);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].security_id, 11);
        assert_eq!(out[0].underlying_id, 100);
        assert_eq!(out[0].window_lots_milli, 5_000);
        assert_eq!(out[0].segment, FNO);
    }

    #[test]
    fn candidates_from_an_empty_ranking_is_empty_not_a_panic() {
        assert!(candidates_from_ranked(&[]).is_empty());
    }

    #[test]
    fn key_carries_the_segment_so_two_segments_never_collide() {
        // I-P1-11. Dhan reuses id 27 across segments; a bare-id key would make
        // an index option answer for a stock option.
        let stock = candidate(27, 100, 1);
        let index = Depth200Candidate {
            segment: IDX,
            ..candidate(27, 100, 1)
        };
        assert_ne!(stock.key(), index.key());
    }

    #[test]
    fn is_aligned_reports_true_when_the_pool_matches_the_ranking() {
        let ranked = vec![candidate(1, 100, 9), candidate(2, 200, 8)];
        let held = [(1_u64, FNO), (2_u64, FNO)];
        let d = diverge(&held, &ranked);
        assert!(d.is_aligned());
        assert_eq!(d.held_total, 2);
    }

    #[test]
    fn diverge_names_every_held_socket_that_is_off_the_ranking() {
        // The production shape as of 2026-09-08: index options hold sockets the
        // ranking says belong to stock options.
        let ranked = vec![candidate(1, 100, 9), candidate(2, 200, 8)];
        let held = [(90_u64, IDX), (91_u64, IDX), (1_u64, FNO)];
        let d = diverge(&held, &ranked);
        assert!(!d.is_aligned());
        assert_eq!(d.held_off_ranking, vec![(90, IDX), (91, IDX)]);
        assert_eq!(d.ranked_unheld, vec![(2, FNO)]);
        assert_eq!(d.held_total, 3);
    }

    #[test]
    fn diverge_keys_on_the_composite_so_a_segment_collision_is_still_a_divergence() {
        // The near miss that a bare-id comparison would report as ALIGNED, which
        // would hide exactly the index-option-on-a-stock-socket state this
        // change exists to surface.
        let ranked = vec![candidate(27, 100, 9)];
        let held = [(27_u64, IDX)];
        let d = diverge(&held, &ranked);
        assert_eq!(d.held_off_ranking, vec![(27, IDX)]);
        assert_eq!(d.ranked_unheld, vec![(27, FNO)]);
    }

    #[test]
    fn an_empty_ranking_makes_every_held_socket_diverge_but_holds_no_wish_list() {
        // Reachable pre-open, when no contract has traded. It is a real reading
        // and must not be confused with the not-yet-ranked state, which
        // `report_divergence` refuses to compare at all.
        let held = [(1_u64, FNO), (2_u64, FNO)];
        let d = diverge(&held, &[]);
        assert_eq!(d.held_off_ranking.len(), 2);
        assert!(d.ranked_unheld.is_empty());
    }

    #[test]
    fn an_empty_pool_leaves_the_whole_ranking_unheld() {
        let ranked = vec![candidate(1, 100, 9)];
        let d = diverge(&[], &ranked);
        assert!(d.held_off_ranking.is_empty());
        assert_eq!(d.ranked_unheld, vec![(1, FNO)]);
        assert_eq!(d.held_total, 0);
    }

    #[test]
    fn report_divergence_refuses_to_compare_before_the_first_ranking() {
        // The false-alarm guard, asserted rather than assumed: five held
        // sockets and no ranking must yield NO divergence claim.
        let view = Depth200Candidates::new();
        let held = [(1_u64, FNO), (2_u64, FNO), (3_u64, FNO)];
        assert!(
            report_divergence(&view, &held).is_none(),
            "nothing has been ranked, so nothing can be said to diverge from it"
        );
    }

    #[test]
    fn report_divergence_compares_once_a_ranking_exists() {
        let view = Depth200Candidates::new();
        view.publish(vec![candidate(1, 100, 9)]);
        let held = [(1_u64, FNO), (2_u64, FNO)];
        let d = report_divergence(&view, &held).expect("a ranking exists");
        assert_eq!(d.held_off_ranking, vec![(2, FNO)]);
    }

    #[test]
    fn report_divergence_on_an_empty_published_ranking_is_a_real_zero_length_answer() {
        // `Some(vec![])` is a ranking. Every held socket diverges from it, and
        // that IS the honest reading — unlike the `None` case above.
        let view = Depth200Candidates::new();
        view.publish(Vec::new());
        let held = [(1_u64, FNO)];
        let d = report_divergence(&view, &held).expect("an empty ranking is still a ranking");
        assert_eq!(d.held_off_ranking, vec![(1, FNO)]);
    }

    /// The hysteresis band: the published list is entry set + band, and the
    /// two halves are read by different questions.
    #[test]
    fn entry_set_is_the_first_budget_rows_and_the_rest_is_the_band() {
        let ranked: Vec<_> = (1..=DEPTH200_EXIT_UNDERLYINGS as u64)
            .map(|i| candidate(i, 10 * i, 1000 - i))
            .collect();
        assert_eq!(entry_set(&ranked).len(), DEPTH_200_SOCKET_BUDGET);
        assert_eq!(entry_set(&ranked)[0].security_id, 1);
        // A short list is its own entry set — no panic, no padding.
        let short = [candidate(1, 10, 9), candidate(2, 20, 8)];
        assert_eq!(entry_set(&short).len(), 2);
        assert!(entry_set(&[]).is_empty());
    }

    #[test]
    fn diverge_treats_a_band_contract_as_on_the_ranking_and_never_as_unheld() {
        let ranked: Vec<_> = (1..=DEPTH200_EXIT_UNDERLYINGS as u64)
            .map(|i| candidate(i, 10 * i, 1000 - i))
            .collect();
        // Holding ranks 1..4 and the first BAND row (6); entry rank 5 unheld.
        let held = [(1_u64, FNO), (2, FNO), (3, FNO), (4, FNO), (6, FNO)];
        let d = diverge(&held, &ranked);
        assert!(
            d.held_off_ranking.is_empty(),
            "a held band contract is not off the ranking: {:?}",
            d.held_off_ranking
        );
        assert_eq!(
            d.ranked_unheld,
            vec![(5, FNO)],
            "only ENTRY rows can be 'should hold'; band rows 7 and 8 must not appear"
        );
    }

    #[test]
    fn global_depth200_candidates_is_one_view() {
        // Two views would mean the drain publishes into one and the steering
        // loop reads the other, which fails silently as "never ranked".
        let a = global_depth200_candidates();
        let b = global_depth200_candidates();
        assert!(Arc::ptr_eq(a, b));
    }
}
