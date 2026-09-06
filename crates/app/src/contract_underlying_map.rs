//! Which underlying a contract belongs to — the one lookup the ranking needs
//! and the drain does not have.
//!
//! ## Why this exists
//!
//! `volume_leaderboard::RankedContract` carries an `underlying_id`, and it is
//! load-bearing twice: the depth-200 selector requires five DISTINCT
//! underlyings, and gainer eligibility is judged per underlying. A tick
//! carries neither — it has the contract's own id and nothing else.
//!
//! ## Where the answer comes from, and why no new fetch is needed
//!
//! The per-minute option-chain REST leg already stores BOTH ids on every row
//! it writes (`option_chain_1m.underlying_security_id` and
//! `.contract_security_id`), and it is already running. That is the mapping,
//! produced once a minute by a leg whose output `dhan_depth_universe` also
//! reads. This module is the in-memory shape of it.
//!
//! **No new REST call, no new table, no rule-file edit.** `/marketfeed/quote`
//! stays FORBIDDEN (`no-rest-except-live-feed-2026-06-27.md` §11.3) and is not
//! needed; the cheaper route is also the one that moves no rule.
//!
//! ## Why `ArcSwap` and not a lock
//!
//! The writer is the chain leg's task; the reader is the frame drain, on the
//! hot path. The chain leg rebuilds the WHOLE mapping once a minute rather
//! than mutating it, so the natural shape is publish-a-new-snapshot: readers
//! take a lock-free `load()` and never block, and a rebuild never contends
//! with a tick. That is the `token_manager` pattern this repository already
//! uses for the same reason.
//!
//! A `Mutex` would put the drain behind a lock held by a once-a-minute
//! rebuild of ~20,000 entries — small, but on the only task emptying the
//! socket, which is the drain-stall shape that ends in upstream tick loss.
//!
//! ## Complexity
//!
//! Lookup is **O(1) average**: one atomic load plus one hash probe on the
//! I-P1-11 composite key. Zero allocation on the read path — `load()` returns
//! a guard, not a clone of the map. A rebuild is O(n) in the legs and happens
//! once a minute on the chain leg's own task, never on the drain.

use std::collections::HashMap;
use std::sync::Arc;

use arc_swap::ArcSwap;
use tickvault_common::types::ExchangeSegment;

/// The I-P1-11 composite identity of a contract. The bare `security_id` is
/// reused across segments, so keying on it alone would let one segment's
/// contract answer for another's.
pub type ContractKey = (u64, ExchangeSegment);

/// Ceiling on tracked contracts. Matches the authorized subscription universe
/// and every other per-instrument map in the lane, so one figure covers them
/// all and there is no second number to keep in step.
pub const MAX_TRACKED_CONTRACTS: usize = 25_000;

/// Counter for legs refused while building a snapshot.
pub const REFUSED_COUNTER: &str = "tv_contract_underlying_refused_total";

/// Why a leg was left out of a snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LegRefusal {
    /// `contract_security_id` was the parser's absent-field default, or
    /// otherwise not a usable id.
    ZeroOrNegativeContractId,
    /// The underlying's id was missing or not usable.
    ZeroOrNegativeUnderlyingId,
    /// The snapshot is at its ceiling.
    AtCapacity,
}

impl LegRefusal {
    /// Stable label for the refusal counter.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ZeroOrNegativeContractId => "zero_contract_id",
            Self::ZeroOrNegativeUnderlyingId => "zero_underlying_id",
            Self::AtCapacity => "at_capacity",
        }
    }
}

/// One chain leg's two ids, as the chain leg already holds them.
///
/// Deliberately NOT the leg struct itself: this module needs exactly two
/// fields, and taking the whole thing would couple the ranking to the chain
/// leg's row shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LegIds {
    /// The option contract's own security id.
    pub contract_security_id: i64,
    /// The underlying's security id.
    pub underlying_security_id: i64,
    /// The CONTRACT's segment — not the underlying's.
    ///
    /// The chain table stores the UNDERLYING's segment (`IDX_I`), which is
    /// not what a contract is subscribed under. The caller resolves the
    /// contract segment and passes it; getting this wrong would file every
    /// contract under a segment no tick ever arrives on, and every lookup
    /// would miss silently.
    pub contract_segment: ExchangeSegment,
}

/// What building a snapshot produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotBuild {
    /// Contracts in the snapshot.
    pub accepted: usize,
    /// Legs left out, with the reason.
    pub refusals: Vec<(i64, LegRefusal)>,
}

/// Builds a mapping snapshot from one minute's chain legs. Pure.
///
/// A later leg for the same contract overwrites an earlier one — the chain is
/// a snapshot of one minute, so a repeat is a re-observation, not a conflict.
#[must_use]
pub fn build_snapshot(legs: &[LegIds]) -> (HashMap<ContractKey, u64>, SnapshotBuild) {
    let mut map: HashMap<ContractKey, u64> =
        HashMap::with_capacity(legs.len().min(MAX_TRACKED_CONTRACTS));
    let mut refusals = Vec::new();

    for leg in legs {
        // `contract_security_id` defaults to 0 when the vendor omits the
        // field, and `dhan_depth_universe` already filters on the same value
        // for the same reason: a zero id would map a real contract onto
        // instrument 0.
        let Ok(contract_id) = u64::try_from(leg.contract_security_id) else {
            refusals.push((
                leg.contract_security_id,
                LegRefusal::ZeroOrNegativeContractId,
            ));
            continue;
        };
        if contract_id == 0 {
            refusals.push((
                leg.contract_security_id,
                LegRefusal::ZeroOrNegativeContractId,
            ));
            continue;
        }
        let Ok(underlying_id) = u64::try_from(leg.underlying_security_id) else {
            refusals.push((
                leg.contract_security_id,
                LegRefusal::ZeroOrNegativeUnderlyingId,
            ));
            continue;
        };
        if underlying_id == 0 {
            refusals.push((
                leg.contract_security_id,
                LegRefusal::ZeroOrNegativeUnderlyingId,
            ));
            continue;
        }
        let key = (contract_id, leg.contract_segment);
        // An UPDATE to a contract already in the snapshot is always allowed;
        // only a NEW contract can hit the ceiling. Refusing the update would
        // pin a stale underlying against a contract that had moved.
        if map.len() >= MAX_TRACKED_CONTRACTS && !map.contains_key(&key) {
            refusals.push((leg.contract_security_id, LegRefusal::AtCapacity));
            continue;
        }
        map.insert(key, underlying_id);
    }

    let accepted = map.len();
    (map, SnapshotBuild { accepted, refusals })
}

/// The published mapping. Cheap to clone — it is one `Arc`.
#[derive(Debug, Clone)]
pub struct ContractUnderlyingMap {
    inner: Arc<ArcSwap<HashMap<ContractKey, u64>>>,
}

impl Default for ContractUnderlyingMap {
    fn default() -> Self {
        Self::new()
    }
}

impl ContractUnderlyingMap {
    /// An empty mapping. Every lookup returns `None` until the first publish.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Arc::new(ArcSwap::from_pointee(HashMap::new())),
        }
    }

    /// Replaces the mapping with a new snapshot, atomically.
    ///
    /// Readers in flight keep the old snapshot until they drop it, so a
    /// rebuild can never hand the drain a half-built map.
    pub fn publish(&self, snapshot: HashMap<ContractKey, u64>) {
        self.inner.store(Arc::new(snapshot));
    }

    /// Builds and publishes in one step, returning what was refused.
    ///
    /// Refusals are counted here rather than in `build_snapshot`, which stays
    /// pure so the decision rule is testable without a metrics recorder.
    pub fn publish_from_legs(&self, legs: &[LegIds]) -> SnapshotBuild {
        let (map, build) = build_snapshot(legs);
        // A refusal means a contract cannot be RANKED at all — the ranking is
        // narrower than the chain, silently, and a counter alone would leave
        // that reaching nobody. One SUMMARY line per publish rather than one
        // per leg: this runs once a minute over a bounded leg count, so the
        // whole picture fits in a single line and a per-leg line would be
        // thousands of them for one event.
        for (_, reason) in &build.refusals {
            metrics::counter!(REFUSED_COUNTER, "reason" => reason.as_str()).increment(1);
        }
        if !build.refusals.is_empty() {
            tracing::warn!(
                refused = build.refusals.len(),
                accepted = build.accepted,
                legs = legs.len(),
                first_reason = build.refusals[0].1.as_str(),
                "contract-to-underlying mapping refused chain legs — those \
                 contracts cannot be ranked or depth-steered this minute \
                 (one line per publish; per-reason counts are on the counter)"
            );
        }
        self.publish(map);
        build
    }

    /// The underlying's id for this contract, or `None`.
    ///
    /// O(1) average: one atomic load, one hash probe. `None` is the honest
    /// answer and the caller must treat it as "cannot rank this contract",
    /// never substitute a zero — a zero underlying would collapse every
    /// unmapped contract onto ONE pseudo-underlying, and the depth-200
    /// distinct-underlying rule would then admit five strikes of nothing.
    #[must_use]
    pub fn underlying_of(&self, contract_id: u64, segment: ExchangeSegment) -> Option<u64> {
        self.inner.load().get(&(contract_id, segment)).copied()
    }

    /// How many contracts the published snapshot holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.load().len()
    }

    /// Whether nothing has been published yet, or the last publish was empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FNO: ExchangeSegment = ExchangeSegment::NseFno;
    const BFO: ExchangeSegment = ExchangeSegment::BseFno;

    fn leg(contract: i64, underlying: i64) -> LegIds {
        LegIds {
            contract_security_id: contract,
            underlying_security_id: underlying,
            contract_segment: FNO,
        }
    }

    #[test]
    fn build_snapshot_maps_every_usable_leg() {
        let (map, build) = build_snapshot(&[leg(100, 13), leg(101, 13), leg(200, 25)]);
        assert_eq!(build.accepted, 3);
        assert!(build.refusals.is_empty());
        assert_eq!(map.get(&(100, FNO)), Some(&13));
        assert_eq!(map.get(&(200, FNO)), Some(&25));
    }

    #[test]
    fn build_snapshot_refuses_the_zero_contract_id_sentinel() {
        // `contract_security_id` defaults to 0 when the vendor omits it, and
        // mapping it would file a real contract onto instrument 0.
        let (map, build) = build_snapshot(&[leg(0, 13), leg(100, 13)]);
        assert_eq!(build.accepted, 1);
        assert_eq!(
            build.refusals,
            vec![(0, LegRefusal::ZeroOrNegativeContractId)]
        );
        assert!(!map.contains_key(&(0, FNO)));
    }

    #[test]
    fn build_snapshot_refuses_a_zero_or_negative_underlying() {
        let (_, build) = build_snapshot(&[leg(100, 0), leg(101, -5)]);
        assert_eq!(build.accepted, 0);
        assert_eq!(build.refusals.len(), 2);
        assert!(
            build
                .refusals
                .iter()
                .all(|(_, r)| *r == LegRefusal::ZeroOrNegativeUnderlyingId)
        );
    }

    #[test]
    fn build_snapshot_refuses_a_negative_contract_id() {
        let (_, build) = build_snapshot(&[leg(-1, 13)]);
        assert_eq!(build.accepted, 0);
        assert_eq!(
            build.refusals,
            vec![(-1, LegRefusal::ZeroOrNegativeContractId)]
        );
    }

    #[test]
    fn build_snapshot_keys_on_the_composite_so_two_segments_never_collide() {
        // I-P1-11. A NIFTY option and a SENSEX option can share a numeric id;
        // without the segment one would answer for the other.
        let legs = [
            LegIds {
                contract_security_id: 500,
                underlying_security_id: 13,
                contract_segment: FNO,
            },
            LegIds {
                contract_security_id: 500,
                underlying_security_id: 51,
                contract_segment: BFO,
            },
        ];
        let (map, build) = build_snapshot(&legs);
        assert_eq!(build.accepted, 2);
        assert_eq!(map.get(&(500, FNO)), Some(&13));
        assert_eq!(map.get(&(500, BFO)), Some(&51));
    }

    #[test]
    fn build_snapshot_lets_a_later_leg_overwrite_an_earlier_one() {
        // One minute's chain is a snapshot; a repeat is a re-observation.
        let (map, build) = build_snapshot(&[leg(100, 13), leg(100, 25)]);
        assert_eq!(build.accepted, 1);
        assert_eq!(map.get(&(100, FNO)), Some(&25));
    }

    #[test]
    fn build_snapshot_refuses_a_new_contract_at_the_ceiling() {
        let mut legs: Vec<LegIds> = (1..=MAX_TRACKED_CONTRACTS as i64)
            .map(|i| leg(i, 13))
            .collect();
        legs.push(leg(999_999, 13));
        let (map, build) = build_snapshot(&legs);
        assert_eq!(map.len(), MAX_TRACKED_CONTRACTS);
        assert_eq!(build.refusals, vec![(999_999, LegRefusal::AtCapacity)]);
    }

    #[test]
    fn build_snapshot_still_updates_a_tracked_contract_at_the_ceiling() {
        // The ceiling bounds how many contracts are held, never how fresh
        // their mapping is. Refusing this would pin a stale underlying.
        let mut legs: Vec<LegIds> = (1..=MAX_TRACKED_CONTRACTS as i64)
            .map(|i| leg(i, 13))
            .collect();
        legs.push(leg(7, 25));
        let (map, build) = build_snapshot(&legs);
        assert_eq!(map.get(&(7, FNO)), Some(&25));
        assert!(build.refusals.is_empty());
    }

    #[test]
    fn underlying_of_returns_none_before_the_first_publish() {
        // None, never 0 — a zero underlying would collapse every unmapped
        // contract onto ONE pseudo-underlying, and the depth-200
        // distinct-underlying rule would admit five strikes of nothing.
        let m = ContractUnderlyingMap::new();
        assert!(m.is_empty());
        assert_eq!(m.underlying_of(100, FNO), None);
    }

    #[test]
    fn publish_from_legs_makes_the_mapping_readable_and_reports_refusals() {
        let m = ContractUnderlyingMap::new();
        let build = m.publish_from_legs(&[leg(100, 13), leg(0, 13)]);
        assert_eq!(build.accepted, 1);
        assert_eq!(build.refusals.len(), 1);
        assert_eq!(m.underlying_of(100, FNO), Some(13));
        assert_eq!(m.len(), 1);
    }

    #[test]
    fn publish_replaces_rather_than_merges() {
        // The chain leg rebuilds the WHOLE mapping each minute. A contract
        // that has left the chain (an expired strike) must LEAVE the map, or
        // the ranking keeps steering depth at a contract nobody trades.
        let m = ContractUnderlyingMap::new();
        m.publish_from_legs(&[leg(100, 13), leg(101, 13)]);
        assert_eq!(m.len(), 2);
        m.publish_from_legs(&[leg(101, 13)]);
        assert_eq!(m.len(), 1);
        assert_eq!(m.underlying_of(100, FNO), None);
        assert_eq!(m.underlying_of(101, FNO), Some(13));
    }

    #[test]
    fn a_clone_shares_the_same_published_mapping() {
        // The drain holds a clone; the chain leg holds the original. A
        // publish on one must be visible to the other, or the drain reads an
        // empty map forever.
        let writer = ContractUnderlyingMap::new();
        let reader = writer.clone();
        writer.publish_from_legs(&[leg(100, 13)]);
        assert_eq!(reader.underlying_of(100, FNO), Some(13));
    }

    #[test]
    fn as_str_labels_are_distinct_for_every_refusal_reason() {
        let labels = [
            LegRefusal::ZeroOrNegativeContractId.as_str(),
            LegRefusal::ZeroOrNegativeUnderlyingId.as_str(),
            LegRefusal::AtCapacity.as_str(),
        ];
        let mut sorted = labels;
        sorted.sort_unstable();
        sorted.iter().zip(sorted.iter().skip(1)).for_each(|(a, b)| {
            assert_ne!(a, b, "refusal labels must be distinct: {labels:?}");
        });
    }
}
