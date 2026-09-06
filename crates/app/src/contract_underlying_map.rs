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
//! ## Where the answer comes from: the daily MASTER, at attach
//!
//! ⚠ **CORRECTED 2026-09-06, before this module ever ran.** The first version
//! of this header named the per-minute option-chain REST leg as the source —
//! *"the chain leg already stores BOTH ids on every row it writes"* — and that
//! is TRUE of the rows it writes and USELESS for the family that matters.
//!
//! `CHAIN_1M_UNDERLYINGS` (`constants.rs`) is a **const-asserted 3-element
//! array**: NIFTY=13, BANKNIFTY=25, SENSEX=51. Every one is an INDEX. So
//! `option_chain_1m` holds only index-option legs — roughly 1,250 contracts —
//! and holds NOTHING for the ~20,220 STOCK options, which are the **only**
//! family the 2026-09-06 depth lock subscribes (*"for depth 20 and depth 200
//! only stocks options contracts strikes dude okay? No underlying spot or
//! futures or indices or indices fmo"*).
//!
//! Wired that way this map would have answered for exactly the family that
//! must never reach depth and returned `None` for the family that must. The
//! stock leaderboard would have ranked nothing, depth would have subscribed
//! nothing, and no counter would have moved — the false-OK class, inside the
//! change meant to deliver the operator's ask. It is recorded rather than
//! quietly rewritten because the reasoning error is the reusable part: the
//! chain leg IS the right source for `dhan_depth_universe`, whose layout was
//! index ATM pairs, and citing that precedent carried a premise the
//! 2026-09-06 lock had already retired.
//!
//! **The source is the pair of daily artifacts**, which is what the plan's own
//! settled Decision B says (`active-plan-volume-depth-steering.md` — *"the
//! contract artifact, at attach"*) and what `dhan_depth_universe` already
//! joins at attach. `ContractRow` says which contracts exist and what class
//! each is (`OPTIDX`/`OPTSTK` — the family); the mapping artifact resolves an
//! underlying SYMBOL to an id. [`legs_from_artifact`] performs that join.
//!
//! That covers BOTH families, and it is the source that DETERMINES the
//! subscription — so the map cannot drift from what is actually subscribed,
//! which a second, narrower source can and would.
//!
//! **No new REST call, no new table, no rule-file edit.** `/marketfeed/quote`
//! stays FORBIDDEN (`no-rest-except-live-feed-2026-06-27.md` §11.3) and is not
//! needed; the master is already downloaded and parsed every morning.
//!
//! ## Why `ArcSwap` and not a lock
//!
//! The writer is the attach path; the reader is the frame drain, on the hot
//! path. The attach rebuilds the WHOLE mapping rather than mutating it, so the
//! natural shape is publish-a-new-snapshot: readers take a lock-free `load()`
//! and never block, and a rebuild never contends with a tick. That is the
//! `token_manager` pattern this repository already uses for the same reason.
//!
//! A `Mutex` would put the drain behind a lock held by a rebuild of ~21,000
//! entries — small, but on the only task emptying the socket, which is the
//! drain-stall shape that ends in upstream tick loss.
//!
//! ## Complexity
//!
//! Lookup is **O(1) average**: one atomic load plus one hash probe on the
//! I-P1-11 composite key. Zero allocation on the read path — `load()` returns
//! a guard, not a clone of the map. A rebuild is O(n) in the legs and happens
//! ONCE PER DAY on the attach path, never on the drain.
//!
//! [`legs_from_artifact`] is ONE O(n) pass over the contract rows with one
//! hash probe each — never a per-contract scan for its underlying, which would
//! be O(contracts x underlyings).

use std::collections::HashMap;
use std::sync::Arc;

use arc_swap::ArcSwap;
use tickvault_common::types::ExchangeSegment;

use crate::dhan_contract_universe::ContractRow;
use crate::volume_leaderboard::OptionFamily;

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
    /// The underlying SYMBOL is not in the mapping artifact, so the contract
    /// cannot be grouped. Expected in small numbers on any day the two
    /// artifacts were built from different masters.
    UnresolvedUnderlyingSymbol,
    /// The contract is on an exchange this lane does not subscribe (BSE and
    /// everything beyond NSE derivatives).
    UnsupportedSegment,
    /// The underlying resolved, but to the WRONG CLASS — an index option
    /// pointing at an equity, or a stock option at an index. The mapping
    /// artifact is a union keyed on symbol, so a name present as both
    /// resolves by file order; refusing is what stops that order deciding a
    /// a contract's grouping.
    UnderlyingClassMismatch,
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
            Self::UnresolvedUnderlyingSymbol => "unresolved_underlying_symbol",
            Self::UnsupportedSegment => "unsupported_segment",
            Self::UnderlyingClassMismatch => "underlying_class_mismatch",
            Self::AtCapacity => "at_capacity",
        }
    }
}

/// One option contract's identity, reduced to what the ranking needs.
///
/// Deliberately NOT a master row or a chain row: this module needs exactly
/// four fields, and taking a whole vendor row would couple the ranking to
/// whichever producer happened to be wired first — which is the coupling that
/// produced the corrected source claim in this module's header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LegIds {
    /// The option contract's own security id.
    pub contract_security_id: i64,
    /// The underlying's security id.
    pub underlying_security_id: i64,
    /// The CONTRACT's segment — not the underlying's.
    ///
    /// The two genuinely differ: an index option's underlying sits in
    /// `IDX_I` while the contract is subscribed under `NSE_FNO`, and a stock
    /// option's underlying is `NSE_EQ`. Filing a contract under its
    /// underlying's segment would put every entry under a segment no tick
    /// ever arrives on, and every lookup would miss SILENTLY (I-P1-11).
    pub contract_segment: ExchangeSegment,
    /// Which leaderboard this contract belongs in.
    ///
    /// Carried on the SAME row as the underlying id, and that is the point:
    /// the two facts come from one master row, so they cannot disagree. A
    /// second, separately-built family map could drift from this one, and the
    /// drift would be invisible — a stock option ranked as an index option
    /// simply never reaches depth.
    pub family: OptionFamily,
}

/// What the map answers: who a contract belongs to, and which board it is on.
///
/// One value rather than two maps, so the per-tick path is ONE hash probe and
/// the two facts are physically incapable of disagreeing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContractOwner {
    /// The underlying's security id — the depth-200 distinct-underlying key.
    pub underlying_id: u64,
    /// Which leaderboard the contract is ranked on.
    pub family: OptionFamily,
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
pub fn build_snapshot(legs: &[LegIds]) -> (HashMap<ContractKey, ContractOwner>, SnapshotBuild) {
    let mut map: HashMap<ContractKey, ContractOwner> =
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
        map.insert(
            key,
            ContractOwner {
                underlying_id,
                family: leg.family,
            },
        );
    }

    let accepted = map.len();
    (map, SnapshotBuild { accepted, refusals })
}

/// Turns the two daily artifacts into ranking legs — the ONLY producer wired
/// to production, and the correction recorded in this module's header.
///
/// # Why these two inputs
///
/// This is the join `dhan_depth_universe::load_depth_candidates` already
/// performs at attach, on the same two artifacts, for the same reason: the
/// contract artifact says WHICH contracts exist and what class each is, and
/// the mapping artifact resolves an underlying SYMBOL to an id. Neither
/// answers alone.
///
/// Reusing the production pair rather than re-parsing the master is what keeps
/// the ranking map and the SUBSCRIPTION derived from one source. A separately
/// parsed master would be a second derivation that can drift, and the drift
/// would be invisible — a contract ranked under an underlying the subscription
/// path does not use simply never groups correctly.
///
/// # The class check is a refusal, not a preference
///
/// `parse_symbol_map` is a UNION of the index mappings and the F&O underlying
/// mappings, and it inserts by symbol — so a name present as both an index and
/// an equity resolves to whichever the artifact listed last. Rather than
/// inherit that file-order dependency, this checks the segment the map carries
/// against the contract's own class: an `OPTIDX` must resolve to an `IDX_I`
/// underlying and an `OPTSTK` to an `NSE_EQ` one. A mismatch is REFUSED and
/// counted, never accepted — a contract grouped under the wrong underlying
/// breaks the depth-200 distinct-underlying rule silently, which is worse than
/// one contract missing from the board.
///
/// # Complexity
///
/// One O(n) pass over the contract rows with one hash probe each. Never a scan
/// per contract, which would be O(contracts x underlyings).
#[must_use]
pub fn legs_from_artifact(
    contracts: &[ContractRow],
    symbols: &HashMap<String, (u64, u8)>,
) -> (Vec<LegIds>, Vec<(i64, LegRefusal)>) {
    let idx_code = ExchangeSegment::IdxI.binary_code();
    let eq_code = ExchangeSegment::NseEquity.binary_code();

    let mut legs = Vec::new();
    let mut refusals = Vec::new();
    for row in contracts {
        let family = match row.c.as_str() {
            "OPTIDX" => OptionFamily::Index,
            "OPTSTK" => OptionFamily::Stock,
            // Futures and anything else are not ranked. Skipped SILENTLY and
            // never counted: they are the ordinary contents of the artifact,
            // not a defect, and counting them would make the refusal counter
            // measure the file's shape instead of a problem.
            _ => continue,
        };
        let contract_id = match i64::try_from(row.i) {
            Ok(id) if id > 0 => id,
            // A zero id is the parser's "absent or unusable" answer. Mapping a
            // real contract onto instrument 0 would look healthy and group
            // every such contract together.
            _ => {
                refusals.push((0, LegRefusal::ZeroOrNegativeContractId));
                continue;
            }
        };
        // BSE derivatives return `None` and are REFUSED, not defaulted. The
        // selector narrowed to NSE on 2026-08-20 and depth is NSE-only at the
        // vendor, so a BSE contract filed under an NSE segment would be an
        // entry no tick can ever match — silently.
        let Some(contract_segment) = crate::dhan_contract_universe::derivative_segment(&row.x)
        else {
            refusals.push((contract_id, LegRefusal::UnsupportedSegment));
            continue;
        };
        // Same normalization `parse_symbol_map` applied when it built the map.
        // Looking up an un-normalized symbol would miss every entry whose
        // source row carried different case or padding.
        let Some(&(underlying_id, underlying_segment)) =
            symbols.get(row.u.trim().to_uppercase().as_str())
        else {
            refusals.push((contract_id, LegRefusal::UnresolvedUnderlyingSymbol));
            continue;
        };
        let expected = match family {
            OptionFamily::Index => idx_code,
            OptionFamily::Stock => eq_code,
        };
        if underlying_segment != expected {
            refusals.push((contract_id, LegRefusal::UnderlyingClassMismatch));
            continue;
        }
        let Ok(underlying_security_id) = i64::try_from(underlying_id) else {
            refusals.push((contract_id, LegRefusal::ZeroOrNegativeUnderlyingId));
            continue;
        };
        if underlying_security_id <= 0 {
            refusals.push((contract_id, LegRefusal::ZeroOrNegativeUnderlyingId));
            continue;
        }
        legs.push(LegIds {
            contract_security_id: contract_id,
            underlying_security_id,
            contract_segment,
            family,
        });
    }
    (legs, refusals)
}
/// The published mapping. Cheap to clone — it is one `Arc`.
#[derive(Debug, Clone)]
pub struct ContractUnderlyingMap {
    inner: Arc<ArcSwap<HashMap<ContractKey, ContractOwner>>>,
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
    pub fn publish(&self, snapshot: HashMap<ContractKey, ContractOwner>) {
        self.inner.store(Arc::new(snapshot));
    }

    /// Builds and publishes in one step, returning what was refused.
    ///
    /// Refusals are counted here rather than in `build_snapshot`, which stays
    /// pure so the decision rule is testable without a metrics recorder.
    pub fn publish_from_legs(&self, legs: &[LegIds]) -> SnapshotBuild {
        let (map, build) = build_snapshot(legs);
        // A refusal means a contract cannot be RANKED at all — the ranking is
        // narrower than the subscription, silently, and a counter alone would
        // leave that reaching nobody. One SUMMARY line per publish, not per leg.
        // per leg: this runs ONCE PER DAY at attach over a bounded leg count, so
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
                 contracts cannot be ranked or depth-steered today \
                 (one line per publish; per-reason counts are on the counter)"
            );
        }
        self.publish(map);
        build
    }

    /// Who this contract belongs to and which board it ranks on, or `None`.
    ///
    /// **This is the per-tick call**, and `None` is the common, correct answer:
    /// the main feed carries ~870 spots and ~660 futures alongside the options,
    /// and none of them is in this map. The caller SKIPS an absent contract; it
    /// must never be counted as a refusal, or the refusal counter measures the
    /// ordinary shape of the feed instead of a defect.
    ///
    /// O(1) average: one atomic load, one hash probe. A caller must never
    /// substitute a zero underlying for `None` — that would collapse every
    /// unmapped contract onto ONE pseudo-underlying, and the depth-200
    /// distinct-underlying rule would then admit five strikes of nothing.
    #[must_use]
    pub fn owner_of(&self, contract_id: u64, segment: ExchangeSegment) -> Option<ContractOwner> {
        self.inner.load().get(&(contract_id, segment)).copied()
    }

    /// The underlying's id alone, for callers that do not need the family.
    ///
    /// A thin projection of [`Self::owner_of`] rather than a second lookup
    /// path, so the two can never answer differently.
    #[must_use]
    pub fn underlying_of(&self, contract_id: u64, segment: ExchangeSegment) -> Option<u64> {
        self.owner_of(contract_id, segment)
            .map(|owner| owner.underlying_id)
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
            family: OptionFamily::Stock,
        }
    }

    /// Projects the owner back to the bare underlying id, so the existing
    /// assertions keep reading as "which underlying did this land under".
    fn under(map: &HashMap<ContractKey, ContractOwner>, key: ContractKey) -> Option<u64> {
        map.get(&key).map(|o| o.underlying_id)
    }

    #[test]
    fn build_snapshot_maps_every_usable_leg() {
        let (map, build) = build_snapshot(&[leg(100, 13), leg(101, 13), leg(200, 25)]);
        assert_eq!(build.accepted, 3);
        assert!(build.refusals.is_empty());
        assert_eq!(under(&map, (100, FNO)), Some(13));
        assert_eq!(under(&map, (200, FNO)), Some(25));
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
                family: OptionFamily::Stock,
            },
            LegIds {
                contract_security_id: 500,
                underlying_security_id: 51,
                contract_segment: BFO,
                family: OptionFamily::Index,
            },
        ];
        let (map, build) = build_snapshot(&legs);
        assert_eq!(build.accepted, 2);
        assert_eq!(under(&map, (500, FNO)), Some(13));
        assert_eq!(under(&map, (500, BFO)), Some(51));
    }

    #[test]
    fn build_snapshot_lets_a_later_leg_overwrite_an_earlier_one() {
        // One minute's chain is a snapshot; a repeat is a re-observation.
        let (map, build) = build_snapshot(&[leg(100, 13), leg(100, 25)]);
        assert_eq!(build.accepted, 1);
        assert_eq!(under(&map, (100, FNO)), Some(25));
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
        assert_eq!(under(&map, (7, FNO)), Some(25));
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

    // ---------------------------------------------------------------------
    // legs_from_artifact — the corrected producer
    // ---------------------------------------------------------------------

    fn contract(id: u64, class: &str, underlying: &str, exch: &str) -> ContractRow {
        ContractRow {
            i: id,
            x: exch.to_owned(),
            c: class.to_owned(),
            e: 20_260_925,
            s: 2_500_000,
            l: "CE".to_owned(),
            u: underlying.to_owned(),
        }
    }

    /// The mapping artifact as production builds it: a UNION of the index
    /// mappings (`IDX_I`) and the F&O underlying mappings (`NSE_EQ`).
    fn symbol_map() -> HashMap<String, (u64, u8)> {
        let idx = ExchangeSegment::IdxI.binary_code();
        let eq = ExchangeSegment::NseEquity.binary_code();
        HashMap::from([
            ("NIFTY".to_owned(), (13_u64, idx)),
            ("BANKNIFTY".to_owned(), (25_u64, idx)),
            ("RELIANCE".to_owned(), (2885_u64, eq)),
            ("TCS".to_owned(), (11536_u64, eq)),
        ])
    }

    #[test]
    fn legs_from_artifact_resolves_both_families() {
        let rows = [
            contract(500, "OPTIDX", "NIFTY", "NSE"),
            contract(600, "OPTSTK", "RELIANCE", "NSE"),
        ];
        let (legs, refusals) = legs_from_artifact(&rows, &symbol_map());
        assert!(refusals.is_empty(), "unexpected refusals: {refusals:?}");
        assert_eq!(legs.len(), 2);
        assert_eq!(legs[0].underlying_security_id, 13);
        assert_eq!(legs[0].family, OptionFamily::Index);
        assert_eq!(legs[1].underlying_security_id, 2885);
        assert_eq!(legs[1].family, OptionFamily::Stock);
        // Both are subscribed under the CONTRACT's segment, never the
        // underlying's — an entry under `IDX_I` or `NSE_EQ` would match no
        // tick this lane ever receives.
        assert!(legs.iter().all(|l| l.contract_segment == FNO));
    }

    /// The NEGATIVE CONTROL for the defect this module's header records.
    ///
    /// `option_chain_1m` covers three INDEX underlyings and cannot represent a
    /// stock symbol at all, so a symbol map derived from it resolves every
    /// index option and NO stock option. This builds exactly that map and
    /// asserts the stock side comes back empty — proving the corrected source
    /// earns its place, rather than asserting that it does.
    #[test]
    fn an_index_only_symbol_map_resolves_zero_stock_options() {
        let idx = ExchangeSegment::IdxI.binary_code();
        let chain_only: HashMap<String, (u64, u8)> = HashMap::from([
            ("NIFTY".to_owned(), (13_u64, idx)),
            ("BANKNIFTY".to_owned(), (25_u64, idx)),
            ("SENSEX".to_owned(), (51_u64, idx)),
        ]);
        let rows = [
            contract(500, "OPTIDX", "NIFTY", "NSE"),
            contract(600, "OPTSTK", "RELIANCE", "NSE"),
            contract(601, "OPTSTK", "TCS", "NSE"),
        ];
        let (legs, refusals) = legs_from_artifact(&rows, &chain_only);
        assert_eq!(
            legs.iter()
                .filter(|l| l.family == OptionFamily::Stock)
                .count(),
            0,
            "an index-only map must resolve no stock option — this is the \
             defect the corrected source removes"
        );
        assert_eq!(legs.len(), 1, "only the index option resolves");
        assert_eq!(refusals.len(), 2);
        assert!(
            refusals
                .iter()
                .all(|(_, r)| *r == LegRefusal::UnresolvedUnderlyingSymbol)
        );
    }

    #[test]
    fn legs_from_artifact_refuses_a_class_mismatch_rather_than_grouping_wrongly() {
        // The mapping artifact is a union keyed on symbol, so a name present
        // as BOTH an index and an equity resolves by file order. Refusing is
        // what stops that order silently deciding a contract's grouping.
        let rows = [contract(500, "OPTIDX", "RELIANCE", "NSE")];
        let (legs, refusals) = legs_from_artifact(&rows, &symbol_map());
        assert!(legs.is_empty());
        assert_eq!(refusals, vec![(500, LegRefusal::UnderlyingClassMismatch)]);
    }

    #[test]
    fn legs_from_artifact_refuses_a_bse_contract_rather_than_defaulting_it() {
        // Depth is NSE-only at the vendor and the selector narrowed to NSE on
        // 2026-08-20. Filing a BSE contract under an NSE segment would be an
        // entry no tick can ever match.
        let rows = [contract(700, "OPTIDX", "NIFTY", "BSE")];
        let (legs, refusals) = legs_from_artifact(&rows, &symbol_map());
        assert!(legs.is_empty());
        assert_eq!(refusals, vec![(700, LegRefusal::UnsupportedSegment)]);
    }

    #[test]
    fn legs_from_artifact_skips_non_options_silently_and_never_counts_them() {
        // Futures are the ordinary contents of the artifact, not a defect.
        // Counting them would make the refusal counter measure the file's
        // shape instead of a problem.
        let rows = [
            contract(800, "FUTIDX", "NIFTY", "NSE"),
            contract(801, "FUTSTK", "RELIANCE", "NSE"),
            contract(500, "OPTIDX", "NIFTY", "NSE"),
        ];
        let (legs, refusals) = legs_from_artifact(&rows, &symbol_map());
        assert_eq!(legs.len(), 1);
        assert!(
            refusals.is_empty(),
            "futures must not be counted as refusals"
        );
    }

    #[test]
    fn legs_from_artifact_normalizes_the_symbol_the_way_the_map_was_built() {
        // `parse_symbol_map` stores `symbol.trim().to_uppercase()`. Looking up
        // an un-normalized symbol would miss every entry whose source row
        // carried different case or padding.
        let rows = [contract(600, "OPTSTK", "  reliance  ", "NSE")];
        let (legs, refusals) = legs_from_artifact(&rows, &symbol_map());
        assert!(refusals.is_empty(), "unexpected refusals: {refusals:?}");
        assert_eq!(legs.len(), 1);
        assert_eq!(legs[0].underlying_security_id, 2885);
    }

    #[test]
    fn legs_from_artifact_refuses_the_zero_contract_id_sentinel() {
        let rows = [contract(0, "OPTSTK", "RELIANCE", "NSE")];
        let (legs, refusals) = legs_from_artifact(&rows, &symbol_map());
        assert!(legs.is_empty());
        assert_eq!(refusals, vec![(0, LegRefusal::ZeroOrNegativeContractId)]);
    }

    #[test]
    fn owner_of_and_underlying_of_can_never_disagree() {
        // `underlying_of` is a PROJECTION of `owner_of`, not a second lookup
        // path. Pinned because two independent lookups over the same map is
        // exactly the drift this module consolidated into one value to avoid.
        let rows = [contract(600, "OPTSTK", "RELIANCE", "NSE")];
        let (legs, _) = legs_from_artifact(&rows, &symbol_map());
        let map = ContractUnderlyingMap::new();
        map.publish_from_legs(&legs);

        assert_eq!(
            map.owner_of(600, FNO).map(|o| o.underlying_id),
            map.underlying_of(600, FNO)
        );
        assert_eq!(map.underlying_of(600, FNO), Some(2885));

        // An absent contract: both answer `None`, and the caller SKIPS rather
        // than counting a refusal — spots and futures land here legitimately.
        assert!(map.owner_of(999, FNO).is_none());
        assert!(map.underlying_of(999, FNO).is_none());
    }

    #[test]
    fn legs_from_artifact_feeds_a_map_that_answers_both_families() {
        // End to end: artifact -> legs -> published map -> per-tick lookup.
        let rows = [
            contract(500, "OPTIDX", "NIFTY", "NSE"),
            contract(600, "OPTSTK", "RELIANCE", "NSE"),
        ];
        let (legs, _) = legs_from_artifact(&rows, &symbol_map());
        let map = ContractUnderlyingMap::new();
        map.publish_from_legs(&legs);

        let stock = map.owner_of(600, FNO).expect("stock option must resolve");
        assert_eq!(stock.underlying_id, 2885);
        assert_eq!(stock.family, OptionFamily::Stock);

        let index = map.owner_of(500, FNO).expect("index option must resolve");
        assert_eq!(index.family, OptionFamily::Index);

        // A spot or future the map never held: absent, and the caller SKIPS it
        // rather than counting a refusal.
        assert!(map.owner_of(13, ExchangeSegment::IdxI).is_none());
    }
}
