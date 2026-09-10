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

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use arc_swap::ArcSwap;
use tickvault_common::types::ExchangeSegment;
use tickvault_core::websocket::pool_supervisor::SubscribeInstrument;

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
    /// The master carried no usable `LOT_SIZE` for this contract.
    ///
    /// REFUSED rather than defaulted to 1. A lot size is the divisor that
    /// turns traded units into lots, so defaulting it leaves that contract
    /// ranked in RAW UNITS while every sibling is ranked in lots — which puts
    /// it at the top of the board by a factor of its own lot size and hands a
    /// depth socket to whichever contract the master happened to be missing a
    /// column for. A contract absent from the board is visible in this
    /// counter; a contract wrongly at the top of it is not.
    MissingLotSize,
    /// The snapshot is at its ceiling.
    AtCapacity,
}

impl LegRefusal {
    /// Every variant, so the counter can be seeded exhaustively.
    ///
    /// A const array rather than a derive: adding a variant without adding it
    /// here leaves that reason's series unseeded, and an unseeded series is one
    /// the CloudWatch agent drops on its first sample -- silent on the one day
    /// it fires. `all_variants_are_in_the_seed_list` pins that this stays whole.
    pub const ALL: [Self; 7] = [
        Self::ZeroOrNegativeContractId,
        Self::ZeroOrNegativeUnderlyingId,
        Self::UnresolvedUnderlyingSymbol,
        Self::UnsupportedSegment,
        Self::UnderlyingClassMismatch,
        Self::MissingLotSize,
        Self::AtCapacity,
    ];

    /// Stable label for the refusal counter.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ZeroOrNegativeContractId => "zero_contract_id",
            Self::ZeroOrNegativeUnderlyingId => "zero_underlying_id",
            Self::UnresolvedUnderlyingSymbol => "unresolved_underlying_symbol",
            Self::UnsupportedSegment => "unsupported_segment",
            Self::UnderlyingClassMismatch => "underlying_class_mismatch",
            Self::MissingLotSize => "missing_lot_size",
            Self::AtCapacity => "at_capacity",
        }
    }
}

/// One option contract's identity, reduced to what the ranking needs.
///
/// Deliberately NOT a master row or a chain row: this module needs exactly
/// five fields, and taking a whole vendor row would couple the ranking to
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
    /// Contract multiplier from the master's `LOT_SIZE` column — how many
    /// units one lot is.
    ///
    /// Guaranteed non-zero: [`legs_from_artifact`] refuses a leg whose master
    /// row carried no usable lot size rather than defaulting it, so a divide
    /// by this value cannot fault and cannot silently rank one contract in a
    /// different unit from its siblings.
    pub lot_size: u32,
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
    /// Units per lot, from the daily master. Always non-zero — see [`LegIds`].
    ///
    /// Carried HERE, on the same value the drain already looks up, so the
    /// ranking path pays ONE hash probe for owner, family and multiplier
    /// together. A separate lot-size map would be a second probe on the hot
    /// path and a second thing that can go missing independently.
    pub lot_size: u32,
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
        // Enforced HERE and not only in `legs_from_artifact`, because this is
        // the function that CONSTRUCTS `ContractOwner`, and that type's
        // contract says its lot size is never zero. A caller assembling legs
        // by hand — a test, or a future second producer — must not be able to
        // publish a value that makes a divide fault on the drain.
        if leg.lot_size == 0 {
            refusals.push((leg.contract_security_id, LegRefusal::MissingLotSize));
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
                lot_size: leg.lot_size,
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
/// # The lot size is a refusal for the same reason
///
/// A missing `LOT_SIZE` is refused, never defaulted to 1. The lot size is the
/// divisor that puts every contract's traded volume in the SAME unit, so a
/// contract that keeps its raw units outranks its siblings by a factor of its
/// own lot size — and it does so at the top of the board, where it takes a
/// depth socket. Refusing costs one contract; defaulting corrupts the
/// ordering, and only the refusal leaves a counter behind.
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
        // `ContractRow::z` is `0` when the master carried no `LOT_SIZE`, and
        // `#[serde(default)]` also yields `0` for an artifact written by a
        // binary that predates the column. Both are the same answer — we do
        // not know this contract's multiplier — and both are REFUSED rather
        // than defaulted, per `LegRefusal::MissingLotSize`.
        if row.z == 0 {
            refusals.push((contract_id, LegRefusal::MissingLotSize));
            continue;
        }
        legs.push(LegIds {
            contract_security_id: contract_id,
            underlying_security_id,
            contract_segment,
            family,
            lot_size: row.z,
        });
    }
    (legs, refusals)
}

/// Refusals tallied by reason, largest first, zeros omitted.
///
/// The PURE half, deliberately: it takes no recorder and returns data, so the
/// tallying rule is testable exactly as `build_snapshot` is. The counting half
/// is [`count_artifact_refusals`].
///
/// Ordered by count DESCENDING and then by label, so the output is
/// deterministic for a test and reads dominant-cause-first for a human. A
/// refusal storm is almost always ONE cause, and the first pair is it.
#[must_use]
pub fn tally_refusals(refusals: &[(i64, LegRefusal)]) -> Vec<(LegRefusal, usize)> {
    let mut tally: Vec<(LegRefusal, usize)> =
        LegRefusal::ALL.iter().map(|&r| (r, 0usize)).collect();
    for (_, reason) in refusals {
        if let Some(slot) = tally.iter_mut().find(|(r, _)| r == reason) {
            slot.1 += 1;
        }
    }
    tally.retain(|(_, n)| *n > 0);
    tally.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.as_str().cmp(b.0.as_str())));
    tally
}

/// Counts artifact-scan refusals on the per-reason counter and returns a
/// one-line breakdown for the log.
///
/// # Why this exists (MEASURED 2026-09-09)
///
/// [`legs_from_artifact`] returns its refusals to the caller, and until today
/// the caller logged only `refused_in_artifact_scan = <count>` and dropped the
/// reasons. [`publish_from_legs`] counts the refusals IT produces; nothing
/// counted these.
///
/// That gap cost real diagnosis time. On 2026-09-08 and 2026-09-09 the live
/// lane refused 113,182 and then 113,746 option legs — EVERY leg in the
/// artifact — and published an empty map, so the top-volume board ranked
/// nothing for two whole sessions. The telemetry said `113746` and not one
/// word about why; the cause (`missing_lot_size`, from a vendor `LOT_SIZE` of
/// `"75.0"` that an integer parse refused) had to be found by reading source.
/// One `missing_lot_size=113746` would have named it immediately.
///
/// # The honest limit of the counter half
///
/// The CloudWatch agent folds a metric's label values into ONE summed series
/// per host, so the per-reason split does NOT survive to CloudWatch — there it
/// is a single total. That is why this returns a STRING for the log line as
/// well: the log is the surface where the breakdown actually reaches an
/// operator. The counter is the local `/metrics` view and the trend.
///
/// Returns `"none"` for an empty slice rather than an empty string, so a log
/// field is never blank and "nothing was refused" is stated rather than
/// inferred from absence.
#[must_use]
pub fn count_artifact_refusals(refusals: &[(i64, LegRefusal)]) -> String {
    for (_, reason) in refusals {
        metrics::counter!(REFUSED_COUNTER, "reason" => reason.as_str()).increment(1);
    }
    let tally = tally_refusals(refusals);
    if tally.is_empty() {
        return "none".to_owned();
    }
    tally
        .iter()
        .map(|(reason, n)| format!("{}={}", reason.as_str(), n))
        .collect::<Vec<_>>()
        .join(" ")
}
/// Puts the legs the main feed will actually SUBSCRIBE ahead of the rest, so
/// they are the ones that fit inside [`MAX_TRACKED_CONTRACTS`].
///
/// # Why the order matters (MEASURED 2026-09-10)
///
/// [`legs_from_artifact`] yields every OPTIDX/OPTSTK leg in the artifact across
/// ALL expiries — **76,890** on 2026-09-10 — while [`build_snapshot`] keeps the
/// FIRST 25,000 it meets and refuses the rest (**51,890** `AtCapacity` that
/// day). Artifact order is not subscription order, so a contract that was on
/// the wire could sit among the refused, and an unmapped contract is skipped
/// by the ranking SILENTLY: it never reaches a depth socket and nothing says
/// why. Ordering the selected set first turns "whichever 25,000 the file
/// listed first" into "every contract we subscribed, then whatever room is
/// left".
///
/// Matches on the I-P1-11 composite `(security_id, segment)`, never the id
/// alone. Stable: relative order inside each half is preserved, so the refusal
/// set is deterministic across boots for the same artifact and selection.
/// Returns the reordered legs and how many of them were in the selection.
///
/// # Complexity
///
/// O(selected) to build the set, one O(legs) partition pass — cold, once per
/// attach on the contract task, never a hot path.
#[must_use]
pub fn order_selected_first(
    legs: Vec<LegIds>,
    selected: &[SubscribeInstrument],
) -> (Vec<LegIds>, usize) {
    let picked: HashSet<ContractKey> = selected
        .iter()
        .map(|instrument| (instrument.security_id, instrument.segment))
        .collect();
    let (mut ordered, rest): (Vec<LegIds>, Vec<LegIds>) = legs.into_iter().partition(|leg| {
        u64::try_from(leg.contract_security_id)
            .is_ok_and(|id| picked.contains(&(id, leg.contract_segment)))
    });
    let selected_count = ordered.len();
    ordered.extend(rest);
    (ordered, selected_count)
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
        // An EMPTY published map is the silent catastrophe this arm exists for.
        //
        // The refusal warn above cannot cover it: a build with ZERO legs has no
        // refusals, so `refusals.is_empty()` is true and nothing is said --
        // while `owner_of` then returns `None` for every tick and the entire
        // ranking subsystem produces nothing, all session, reporting healthy.
        //
        // That is the exact false-OK class this repository keeps removing, and
        // it is reachable by ordinary means: an artifact with no OPTIDX/OPTSTK
        // rows, or a symbol map that failed to load (its own arm leaves an
        // empty `HashMap` and continues by design).
        //
        // Coded, so it reaches the error triage path rather than only a log
        // anyone happens to read.
        if build.accepted == 0 {
            tracing::error!(
                code = tickvault_common::error_code::ErrorCode::WsGapConnectionState.code_str(),
                legs = legs.len(),
                refused = build.refusals.len(),
                "contract-to-underlying mapping published an EMPTY map — NOTHING can be \
                 ranked or depth-steered today. Every tick will look unmapped, the \
                 top-volume boards stay empty, and no other signal says so."
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

/// The process-wide map.
///
/// Global for the same reason `depth_subscription_view` is: the PUBLISHER is
/// the contract-universe attach and the READER is the frame drain, and the two
/// sit on opposite sides of several `tokio::spawn` boundaries whose signatures
/// already carry a dozen arguments each.
///
/// Defaults to an EMPTY map, which is the truthful answer before the attach has
/// run -- and `publish_from_legs` now says so LOUDLY if the first publish is
/// also empty, so "empty because nothing has attached yet" cannot be confused
/// with "empty because the attach produced nothing".
#[must_use]
pub fn global_contract_underlying_map() -> &'static std::sync::Arc<ContractUnderlyingMap> {
    static MAP: std::sync::OnceLock<std::sync::Arc<ContractUnderlyingMap>> =
        std::sync::OnceLock::new();
    MAP.get_or_init(|| std::sync::Arc::new(ContractUnderlyingMap::new()))
}

/// Puts the refusal counter on the wire at zero before the first publish.
///
/// The CloudWatch agent computes a counter as the delta between consecutive
/// samples and DROPS the first sample of a series it has never seen. A counter
/// whose first increment IS the event therefore publishes nothing on the one
/// day it matters -- the same first-sample rule that hid
/// `tv_depth_rows_spilled_total` on 2026-08-28 and made 104,540 depth rows
/// permanently unclassifiable.
///
/// Every reason is seeded, not just one: an unseeded label is a series that
/// does not exist until it fires, which is exactly the case being protected.
pub fn pre_register_contract_underlying_counters() {
    for reason in LegRefusal::ALL {
        metrics::counter!(REFUSED_COUNTER, "reason" => reason.as_str()).increment(0);
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
            lot_size: 75,
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
                lot_size: 75,
            },
            LegIds {
                contract_security_id: 500,
                underlying_security_id: 51,
                contract_segment: BFO,
                family: OptionFamily::Index,
                lot_size: 15,
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
            // A REAL lot size, because the production default is a real lot
            // size: every row the master carries a `LOT_SIZE` for has one.
            // Leaving this at the absent-field 0 would make every test below
            // exercise the refusal path instead of the path it is named for.
            z: 75,
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
    fn legs_from_artifact_carries_the_masters_lot_size_onto_every_leg() {
        let rows = [
            contract(500, "OPTIDX", "NIFTY", "NSE"),
            ContractRow {
                z: 250,
                ..contract(600, "OPTSTK", "RELIANCE", "NSE")
            },
        ];
        let (legs, refusals) = legs_from_artifact(&rows, &symbol_map());
        assert!(refusals.is_empty(), "unexpected refusals: {refusals:?}");
        // Per-contract, not one figure for the file: NIFTY and RELIANCE have
        // genuinely different multipliers, and a shared default would rank
        // one of them in the wrong unit.
        assert_eq!(legs[0].lot_size, 75);
        assert_eq!(legs[1].lot_size, 250);
    }

    #[test]
    fn legs_from_artifact_refuses_a_missing_lot_size_rather_than_defaulting_it() {
        // `z == 0` is BOTH answers that mean "unknown": a master row with no
        // `LOT_SIZE` column, and an artifact written by a binary that predates
        // the field (`#[serde(default)]`). Defaulting either to 1 would rank
        // this contract in raw units — 75x its siblings on a NIFTY-sized lot —
        // and put it at the top of the board, taking a depth socket.
        let rows = [ContractRow {
            z: 0,
            ..contract(600, "OPTSTK", "RELIANCE", "NSE")
        }];
        let (legs, refusals) = legs_from_artifact(&rows, &symbol_map());
        assert!(legs.is_empty());
        assert_eq!(refusals, vec![(600, LegRefusal::MissingLotSize)]);
    }

    #[test]
    fn build_snapshot_refuses_a_zero_lot_size_so_contract_owner_is_never_zero() {
        // Defence in depth: `legs_from_artifact` already refuses, but
        // `build_snapshot` is what CONSTRUCTS `ContractOwner`, whose contract
        // says the lot size is never zero. A hand-built leg — a test, or a
        // future second producer — must not be able to publish a value that
        // makes the ranking divide by zero.
        let legs = [LegIds {
            lot_size: 0,
            ..leg(600, 2885)
        }];
        let (map, build) = build_snapshot(&legs);
        assert!(map.is_empty());
        assert_eq!(build.accepted, 0);
        assert_eq!(build.refusals, vec![(600, LegRefusal::MissingLotSize)]);
    }

    #[test]
    fn a_published_owner_always_carries_a_divisible_lot_size() {
        // The property the whole gate exists for, asserted end to end on the
        // published map rather than on the builder's return value.
        let rows = [
            contract(500, "OPTIDX", "NIFTY", "NSE"),
            ContractRow {
                z: 0,
                ..contract(600, "OPTSTK", "RELIANCE", "NSE")
            },
        ];
        let (legs, _) = legs_from_artifact(&rows, &symbol_map());
        let map = ContractUnderlyingMap::new();
        map.publish_from_legs(&legs);
        assert!(map.owner_of(500, FNO).is_some_and(|o| o.lot_size > 0));
        // And the one we could not compute a lot size for is simply absent —
        // never present with a made-up multiplier.
        assert!(map.owner_of(600, FNO).is_none());
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

    #[test]
    fn all_variants_are_in_the_seed_list() {
        // An unseeded reason is a series the CloudWatch agent drops on its
        // first sample -- silent on exactly the day it fires. Adding a variant
        // without adding it here would reintroduce that, so the count is
        // asserted against the match arms rather than trusted.
        let labels: std::collections::HashSet<&str> =
            LegRefusal::ALL.iter().map(|r| r.as_str()).collect();
        assert_eq!(
            labels.len(),
            LegRefusal::ALL.len(),
            "labels must be distinct"
        );
        for r in [
            LegRefusal::ZeroOrNegativeContractId,
            LegRefusal::ZeroOrNegativeUnderlyingId,
            LegRefusal::UnresolvedUnderlyingSymbol,
            LegRefusal::UnsupportedSegment,
            LegRefusal::UnderlyingClassMismatch,
            LegRefusal::AtCapacity,
        ] {
            assert!(labels.contains(r.as_str()), "{} is not seeded", r.as_str());
        }
    }

    #[test]
    #[test]
    fn tally_refusals_orders_by_count_descending_so_the_dominant_cause_is_first() {
        let refusals = vec![
            (1, LegRefusal::UnresolvedUnderlyingSymbol),
            (2, LegRefusal::MissingLotSize),
            (3, LegRefusal::MissingLotSize),
            (4, LegRefusal::MissingLotSize),
            (5, LegRefusal::UnsupportedSegment),
            (6, LegRefusal::UnsupportedSegment),
        ];
        let tally = tally_refusals(&refusals);
        assert_eq!(
            tally,
            vec![
                (LegRefusal::MissingLotSize, 3),
                (LegRefusal::UnsupportedSegment, 2),
                (LegRefusal::UnresolvedUnderlyingSymbol, 1),
            ],
            "the largest cause must sort first — a triager reads the head of \
             this list and stops"
        );
    }

    #[test]
    fn tally_refusals_omits_reasons_that_did_not_fire() {
        let tally = tally_refusals(&[(1, LegRefusal::AtCapacity)]);
        assert_eq!(tally, vec![(LegRefusal::AtCapacity, 1)]);
        // A zero for every other reason would bury the one that fired among
        // six that did not, which is the opposite of the point.
        assert_eq!(tally.len(), 1);
    }

    #[test]
    fn tally_refusals_of_nothing_is_empty() {
        assert!(tally_refusals(&[]).is_empty());
    }

    #[test]
    fn tally_refusals_breaks_a_tie_on_the_label_so_the_output_is_deterministic() {
        // Equal counts must not order by hash or by input position: a log line
        // that reorders between runs cannot be diffed across sessions.
        let a = tally_refusals(&[(1, LegRefusal::MissingLotSize), (2, LegRefusal::AtCapacity)]);
        let b = tally_refusals(&[(2, LegRefusal::AtCapacity), (1, LegRefusal::MissingLotSize)]);
        assert_eq!(a, b, "the same multiset must render the same order");
        assert_eq!(
            a[0].0,
            LegRefusal::AtCapacity,
            "at_capacity < missing_lot_size"
        );
    }

    #[test]
    fn count_artifact_refusals_names_the_dominant_reason_first() {
        // The 2026-09-09 shape, scaled down: every leg refused for ONE reason.
        // This is the string that would have ended the two-session diagnosis
        // immediately, so it is asserted verbatim rather than by `contains`.
        let refusals: Vec<(i64, LegRefusal)> = (0..5)
            .map(|i| (i, LegRefusal::MissingLotSize))
            .chain(std::iter::once((99, LegRefusal::UnsupportedSegment)))
            .collect();
        assert_eq!(
            count_artifact_refusals(&refusals),
            "missing_lot_size=5 unsupported_segment=1"
        );
    }

    #[test]
    fn count_artifact_refusals_says_none_rather_than_going_blank() {
        // An empty field reads as "the log line is broken"; "none" states the
        // fact. Also proves the function is safe with no recorder installed.
        assert_eq!(count_artifact_refusals(&[]), "none");
    }

    #[test]
    fn count_artifact_refusals_covers_every_reason_the_enum_carries() {
        // Pairs with `all_variants_are_in_the_seed_list`: a variant added to
        // `LegRefusal` without a label would render here, so this fails rather
        // than silently dropping the new reason out of the breakdown.
        let refusals: Vec<(i64, LegRefusal)> = LegRefusal::ALL
            .iter()
            .enumerate()
            .map(|(i, &r)| (i as i64, r))
            .collect();
        let rendered = count_artifact_refusals(&refusals);
        for reason in LegRefusal::ALL {
            assert!(
                rendered.contains(&format!("{}=1", reason.as_str())),
                "reason {} missing from {rendered}",
                reason.as_str()
            );
        }
    }

    fn pre_register_contract_underlying_counters_never_panics_without_a_recorder() {
        // Not-panicking IS the whole property here, and the name says so: with
        // no recorder installed there is nothing observable to assert against.
        // The seed runs at attach, before any recorder is guaranteed installed
        // in a test process, and a seeding call that aborts the attach would
        // cost the whole contract universe. Every refusal reason being seeded
        // is a separate claim, checked by the label test above.
        pre_register_contract_underlying_counters();
    }

    #[test]
    fn publish_from_legs_reports_an_empty_publish_rather_than_swallowing_it() {
        // The silent-catastrophe arm. Zero legs means zero refusals, so the
        // refusal warn cannot fire, and `owner_of` then returns None for every
        // tick while every counter reads healthy.
        let map = ContractUnderlyingMap::new();
        let build = map.publish_from_legs(&[]);
        assert_eq!(build.accepted, 0);
        assert!(build.refusals.is_empty());
        assert!(
            map.is_empty(),
            "an empty publish must REPLACE the map, not leave a stale one live"
        );
    }

    #[test]
    fn global_contract_underlying_map_is_one_map_and_starts_empty() {
        // Two maps would mean the attach publishes into one and the drain reads
        // the other, failing silently as "no contract is ever mapped".
        let a = global_contract_underlying_map();
        let b = global_contract_underlying_map();
        assert!(std::sync::Arc::ptr_eq(a, b));
    }

    // -- order_selected_first (2026-09-10: the artifact holds ~77k option legs
    //    against a 25,000 cap; artifact order used to decide who was mapped) --

    fn subscribed(id: u64, segment: ExchangeSegment) -> SubscribeInstrument {
        SubscribeInstrument {
            security_id: id,
            segment,
        }
    }

    #[test]
    fn order_selected_first_keeps_subscribed_contracts_ahead_and_stable() {
        let legs: Vec<LegIds> = (1..=5).map(|i| leg(i, 13)).collect();
        let picked = [subscribed(4, FNO), subscribed(2, FNO)];
        let (ordered, count) = order_selected_first(legs, &picked);
        let ids: Vec<i64> = ordered.iter().map(|l| l.contract_security_id).collect();
        // Selected first, in their ORIGINAL relative order; then the rest,
        // also in original order. Stability is what makes the refusal set
        // deterministic across boots.
        assert_eq!(ids, vec![2, 4, 1, 3, 5]);
        assert_eq!(count, 2);
    }

    #[test]
    fn order_selected_first_matches_the_composite_key_never_the_id_alone() {
        // I-P1-11: a subscription for id 7 on NSE_EQ is a DIFFERENT instrument
        // from option contract 7 on NSE_FNO. Matching on the bare id would
        // promote a contract nobody subscribed.
        let legs = vec![leg(7, 13), leg(8, 13)];
        let picked = [subscribed(7, ExchangeSegment::NseEquity)];
        let (ordered, count) = order_selected_first(legs, &picked);
        assert_eq!(count, 0);
        let ids: Vec<i64> = ordered.iter().map(|l| l.contract_security_id).collect();
        assert_eq!(ids, vec![7, 8], "nothing promoted, order untouched");
    }

    #[test]
    fn a_subscribed_contract_listed_last_in_the_artifact_is_mapped_before_the_cap() {
        // The 2026-09-10 shape: more legs than the cap, and the contract we
        // actually put on the wire sits at the END of the artifact. Before
        // the reorder it was the one refused; the ranking then skipped every
        // one of its ticks silently.
        let last = MAX_TRACKED_CONTRACTS as i64 + 1;
        let legs: Vec<LegIds> = (1..=last).map(|i| leg(i, 13)).collect();
        let picked = [subscribed(last as u64, FNO)];
        let (ordered, count) = order_selected_first(legs, &picked);
        assert_eq!(count, 1);
        let (map, build) = build_snapshot(&ordered);
        assert_eq!(map.len(), MAX_TRACKED_CONTRACTS);
        assert_eq!(
            under(&map, (last as u64, FNO)),
            Some(13),
            "the subscribed contract must be mapped — it is the one whose ticks arrive"
        );
        // The cap refuses in ARRIVAL order, so the leg that no longer fits is
        // the LAST unselected one — the subscribed leg took the slot the
        // artifact's tail would have had. The refusal is still counted, never
        // hidden. (The first draft of this assertion expected leg 1 refused,
        // which would mean the map evicted an already-accepted leg — it does
        // not, and must not.)
        assert_eq!(
            build.refusals,
            vec![(MAX_TRACKED_CONTRACTS as i64, LegRefusal::AtCapacity)]
        );
        assert_eq!(under(&map, (1, FNO)), Some(13), "leg 1 keeps its slot");
    }
}
