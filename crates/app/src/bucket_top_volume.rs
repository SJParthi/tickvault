//! Exact rankings of the same bucket quantities emitted by the candle fold.
//!
//! There is no independent cumulative-volume baseline or sampling timer in
//! this module. An accepted [`CandleVolumeUpdate`] replaces one instrument's
//! candle revision. Stock and index option universes remain separate. The
//! metric is explicit and versioned. The live metric signs the existing whole
//! candle volume by its close versus the frozen previous-bar close. Its sign
//! is never discarded, and an unavailable direction is never a zero.
//!
//! Costs: identity lookup is expected O(1); an existing bucket update is
//! O(log N), using exact rational ordering, plus expected O(1) row lookup;
//! a complete board materialization is expected O(N). Bucket eviction and
//! snapshot reclamation can also cost O(N).
//! Loading a prepared winner checks a fixed number of metadata fields and
//! reads its first row, independently of N. None of these statements is a
//! wall-clock latency guarantee. Memory is O(N * F * B), where F is the
//! admitted Top Volume timeframe count (four) and B is the retention bound.
//! The other six candle frames have no ranking slots or index updates.
//!
//! Completeness means coverage of the explicitly registered option universe
//! for this bucket. It is not proof that the provider delivered every trade.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, OnceLock};

use arc_swap::ArcSwapOption;
use tickvault_common::feed::Feed;
use tickvault_common::tick_types::VolumeQuality;
use tickvault_common::types::ExchangeSegment;
use tickvault_trading::candles::{
    CANDLE_SIGNED_BAR_VS_ONE_LOT_METRIC, CANDLE_SIGNED_NET_VS_ONE_LOT_METRIC, CandleVolumeUpdate,
    TOP_VOLUME_TF_COUNT, TfIndex,
};

use crate::volume_leaderboard::{MAX_TRACKED_CONTRACTS, OptionFamily};

type ContractKey = (u64, u8);

/// Bounds the retained correction window; older buckets belong to history
/// persistence/replay, not an unbounded process-local history index.
pub const MAX_RETAINED_BUCKETS_PER_TIMEFRAME: usize = 8;

/// The score is selected explicitly when an engine is created. Changing it
/// requires a new universe generation and new publications.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BucketVolumeMetric {
    /// Compatibility metric: `100 * (gross_volume / lot_size - 1)`.
    GrossActivityVsOneLotV1,
    /// `100 * (estimated_net_volume / lot_size - 1)`.
    /// Positive/negative/zero refer to a tick-rule estimate, not known
    /// aggressor-side executions. The percentage's zero is **one lot**;
    /// a negative percentage does not by itself mean negative net volume.
    SignedEstimatedNetVsOneLotV2,
    /// `100 * (signed_bar_volume / lot_size - 1)`.
    /// The magnitude is the existing candle volume; direction compares this
    /// candle's close with the prior same-timeframe close frozen at open.
    /// This is bar direction, not estimated or reported aggressor flow.
    SignedBarVolumeVsOneLotV3,
}

impl BucketVolumeMetric {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::GrossActivityVsOneLotV1 => "gross_activity_vs_one_lot_v1",
            Self::SignedEstimatedNetVsOneLotV2 => CANDLE_SIGNED_NET_VS_ONE_LOT_METRIC,
            Self::SignedBarVolumeVsOneLotV3 => CANDLE_SIGNED_BAR_VS_ONE_LOT_METRIC,
        }
    }

    fn numerator(self, row: &BucketRankedContract) -> Option<i128> {
        match self {
            Self::GrossActivityVsOneLotV1 => Some(i128::from(row.gross_volume)),
            Self::SignedEstimatedNetVsOneLotV2 => row.estimated_net_volume.map(i128::from),
            Self::SignedBarVolumeVsOneLotV3 => row.estimated_net_volume.map(i128::from),
        }
    }

    /// Refuse a legacy partial tick-rule quantity presented as whole-bar
    /// volume. Unknown direction is allowed as unavailable, never eligible.
    /// Zero is the explicit equal-close outcome, including nonzero activity.
    fn quantity_is_valid(self, gross: u64, signed: Option<i64>) -> bool {
        if self == Self::SignedBarVolumeVsOneLotV3 && gross > i64::MAX as u64 {
            return false;
        }
        signed.is_none_or(|value| {
            let magnitude = value.unsigned_abs();
            magnitude <= gross
                && (self != Self::SignedBarVolumeVsOneLotV3 || value == 0 || magnitude == gross)
        })
    }
}

/// Immutable metadata for the explicitly admitted universe generation.
/// A lot-size/ownership change requires a replacement engine/generation;
/// a live numerator must not silently acquire a different denominator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BucketContract {
    pub security_id: u64,
    pub segment: ExchangeSegment,
    pub underlying_id: u64,
    pub family: OptionFamily,
    pub lot_size: u32,
    pub instrument_definition_version: u64,
}

impl BucketContract {
    fn key(self) -> ContractKey {
        (self.security_id, self.segment.binary_code())
    }
}

/// Exact candle quantities retained beside the score and identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BucketRankedContract {
    pub security_id: u64,
    pub segment: ExchangeSegment,
    pub underlying_id: u64,
    pub lot_size: u32,
    pub instrument_definition_version: u64,
    pub gross_volume: u64,
    pub estimated_net_volume: Option<i64>,
    /// Candle revision, not the enclosing board's revision.
    pub revision: u64,
    /// Selected candle fold-clock second, never refreshed by publication.
    pub last_observed_secs: u32,
    pub quality: VolumeQuality,
    /// Pinned candle quality flags, carried without reconstruction.
    pub volume_quality: u32,
    pub closed: bool,
}

impl BucketRankedContract {
    fn key(self) -> ContractKey {
        (self.security_id, self.segment.binary_code())
    }

    /// Gross lots in thousandths, rounded down. u128 preserves the full
    /// u64 candle-volume domain; storage narrowing must remain checked.
    #[must_use]
    pub fn gross_lots_milli(self) -> Option<u128> {
        (self.lot_size != 0)
            .then(|| u128::from(self.gross_volume) * 1_000 / u128::from(self.lot_size))
    }

    /// Signed estimated lots in thousandths, truncated toward zero for
    /// display only. Ranking always compares the unrounded ratio.
    #[must_use]
    pub fn estimated_net_lots_milli(self) -> Option<i128> {
        let net = self.estimated_net_volume?;
        if self.lot_size == 0 {
            return None;
        }
        Some(i128::from(net) * 1_000 / i128::from(self.lot_size))
    }

    /// Exact percentage represented as `(numerator, positive denominator)`.
    /// This is a rational number, not an integer percentage or float.
    #[must_use]
    pub fn score_ratio(self, metric: BucketVolumeMetric) -> Option<(i128, u32)> {
        if self.lot_size == 0 {
            return None;
        }
        let numerator = metric.numerator(&self)?;
        Some(((numerator - i128::from(self.lot_size)) * 100, self.lot_size))
    }

    /// Percentage in thousandths, truncated toward zero. It is never the
    /// ordering key. The i128 result must be checked before any i64 ILP cast.
    #[must_use]
    pub fn score_milli_pct(self, metric: BucketVolumeMetric) -> Option<i128> {
        let (numerator, denominator) = self.score_ratio(metric)?;
        Some(numerator * 1_000 / i128::from(denominator))
    }
}

/// Rank key stores the unrounded numerator. The largest multiplication is
/// `(u64::MAX * u32::MAX) < 2^96`; signed i64 inputs need fewer bits. i128
/// therefore compares either admitted metric exactly without overflow.
#[derive(Clone, Copy, Debug)]
struct RankKey {
    numerator: i128,
    lot_size: u32,
    identity: ContractKey,
}

impl RankKey {
    fn for_row(row: &BucketRankedContract, metric: BucketVolumeMetric) -> Option<Self> {
        if row.security_id == 0
            || row.underlying_id == 0
            || row.instrument_definition_version == 0
            || row.revision == 0
            || row.lot_size == 0
            || !row.quality.is_eligible()
            || row.volume_quality != 0
            || !metric.quantity_is_valid(row.gross_volume, row.estimated_net_volume)
        {
            return None;
        }
        Some(Self {
            numerator: metric.numerator(row)?,
            lot_size: row.lot_size,
            identity: row.key(),
        })
    }
}

impl Ord for RankKey {
    fn cmp(&self, other: &Self) -> Ordering {
        // Reverse numeric order; deterministic identity order only for an
        // exactly equal ratio. Neither display rounding nor sign removal.
        (other.numerator * i128::from(self.lot_size))
            .cmp(&(self.numerator * i128::from(other.lot_size)))
            .then_with(|| self.identity.cmp(&other.identity))
    }
}

impl PartialOrd for RankKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for RankKey {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for RankKey {}

/// Coverage is relative to `universe_version`, including registered quiet
/// contracts. A quiet contract needs a canonical zero/unchanged candle update
/// before it is counted; this component never invents missing candles.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BucketCoverage {
    pub expected_contracts: usize,
    pub observed_contracts: usize,
    pub eligible_contracts: usize,
    pub uncertain_contracts: usize,
    pub unavailable_net_contracts: usize,
    pub closed_contracts: usize,
    pub oldest_observed_secs: Option<u32>,
    pub newest_observed_secs: Option<u32>,
    /// Conflicting equal revisions, invalid replacement states or exhausted
    /// revision space invalidate this bucket instead of preserving a green
    /// decision flag beside stale rows. Cleared only with a new bucket/epoch.
    pub integrity_fault: bool,
}

impl BucketCoverage {
    fn structurally_valid(self) -> bool {
        self.observed_contracts <= self.expected_contracts
            && self.eligible_contracts <= self.observed_contracts
            && self.uncertain_contracts <= self.observed_contracts
            && self.unavailable_net_contracts <= self.observed_contracts
            && self.closed_contracts <= self.observed_contracts
            && match (self.oldest_observed_secs, self.newest_observed_secs) {
                (None, None) => self.observed_contracts == 0,
                (Some(oldest), Some(newest)) => self.observed_contracts != 0 && oldest <= newest,
                _ => false,
            }
    }

    #[must_use]
    pub const fn complete_for_declared_universe(self) -> bool {
        self.expected_contracts != 0
            && self.observed_contracts == self.expected_contracts
            && self.eligible_contracts == self.expected_contracts
            && self.uncertain_contracts == 0
            && !self.integrity_fault
    }

    #[must_use]
    pub const fn all_closed(self) -> bool {
        self.expected_contracts != 0 && self.closed_contracts == self.expected_contracts
    }
}

/// One immutable complete materialization, including explicit empty and
/// incomplete states. Loading several slots is not a cross-timeframe atomic
/// transaction. All times are the candle engine's IST wall-clock epoch.
#[derive(Debug)]
pub struct BucketTopVolumeSnapshot {
    pub feed: Feed,
    pub family: OptionFamily,
    pub tf: TfIndex,
    pub session_day: u32,
    pub universe_version: u64,
    pub metric: BucketVolumeMetric,
    pub bucket_start_secs: u32,
    pub bucket_end_secs: u32,
    /// Monotone board revision for this exact bucket/family.
    pub revision: u64,
    pub published_secs: u32,
    pub coverage: BucketCoverage,
    /// Eligible rows only. Coverage exposes every omitted/unknown member.
    pub rows: Vec<BucketRankedContract>,
}

impl BucketTopVolumeSnapshot {
    #[must_use]
    pub fn first(&self) -> Option<&BucketRankedContract> {
        self.rows.first()
    }
}

/// Fixed-size winner publication. It is updated independently of full table
/// materialization, so an ordinary candle update never clones N ranked rows.
/// Every field belongs to one coherent bucket revision.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BucketWinnerSnapshot {
    pub feed: Feed,
    pub family: OptionFamily,
    pub tf: TfIndex,
    pub session_day: u32,
    pub universe_version: u64,
    pub metric: BucketVolumeMetric,
    pub bucket_start_secs: u32,
    pub bucket_end_secs: u32,
    pub revision: u64,
    pub published_secs: u32,
    pub coverage: BucketCoverage,
    pub winner: Option<BucketRankedContract>,
}

impl BucketWinnerSnapshot {
    #[must_use]
    pub const fn first(&self) -> Option<&BucketRankedContract> {
        self.winner.as_ref()
    }
}

/// One atomic, bounded publication of exact bucket winners. The latest live
/// bucket and its retained predecessors share one epoch and are replaced
/// together. Entries contain only a header and one winner, never full rows.
#[derive(Default)]
struct BucketWinnerWindow {
    entries: [Option<Arc<BucketWinnerSnapshot>>; MAX_RETAINED_BUCKETS_PER_TIMEFRAME],
    len: usize,
}

impl BucketWinnerWindow {
    fn push(&mut self, snapshot: Arc<BucketWinnerSnapshot>) -> Result<(), BucketPublishRefusal> {
        let Some(entry) = self.entries.get_mut(self.len) else {
            return Err(BucketPublishRefusal::InvalidSnapshot);
        };
        *entry = Some(snapshot);
        self.len += 1;
        Ok(())
    }

    fn latest(&self) -> Option<&Arc<BucketWinnerSnapshot>> {
        self.len
            .checked_sub(1)
            .and_then(|index| self.entries.get(index))
            .and_then(Option::as_ref)
    }

    fn at(&self, bucket_start_secs: u32) -> Option<&Arc<BucketWinnerSnapshot>> {
        self.entries
            .iter()
            .flatten()
            .find(|snapshot| snapshot.bucket_start_secs == bucket_start_secs)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BucketConfigError {
    InvalidSession,
    InvalidUniverseVersion,
    InvalidRetention,
    InvalidIdentity,
    ZeroLotSize,
    MissingDefinitionVersion,
    DuplicateIdentity,
    FamilyAtCapacity,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BucketUpdateRefusal {
    UnsupportedTimeframe,
    WrongFeed,
    WrongSession,
    UnknownContract,
    InvalidBucket,
    InvalidRevision,
    InvalidObservationTime,
    InvalidSignedVolume,
    MetadataMismatch,
    ExpiredBucket,
    StaleRevision,
    ConflictingRevision,
    ClosedBucketReopened,
    BoardRevisionExhausted,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BucketApplyOutcome {
    Applied,
    Duplicate,
}

const fn family_slot(family: OptionFamily) -> usize {
    match family {
        OptionFamily::Index => 0,
        OptionFamily::Stock => 1,
    }
}

#[derive(Default)]
struct FamilyBucket {
    rows: HashMap<ContractKey, BucketRankedContract>,
    ordered: BTreeSet<RankKey>,
    observation_times: BTreeSet<(u32, ContractKey)>,
    uncertain: usize,
    unavailable_net: usize,
    closed: usize,
    revision: u64,
    integrity_fault: bool,
    cached_winner: Option<BucketRankedContract>,
    oldest_observed_secs: Option<u32>,
    newest_observed_secs: Option<u32>,
}

impl FamilyBucket {
    fn poison(&mut self) {
        self.integrity_fault = true;
        self.revision = self.revision.saturating_add(1);
    }

    fn apply(
        &mut self,
        row: BucketRankedContract,
        metric: BucketVolumeMetric,
    ) -> Result<BucketApplyOutcome, BucketUpdateRefusal> {
        let key = row.key();
        let previous = self.rows.get(&key).copied();
        if let Some(old) = previous {
            if row.revision < old.revision {
                return Err(BucketUpdateRefusal::StaleRevision);
            }
            if row.revision == old.revision {
                if row == old {
                    return Ok(BucketApplyOutcome::Duplicate);
                }
                self.poison();
                return Err(BucketUpdateRefusal::ConflictingRevision);
            }
            if old.closed && !row.closed {
                self.poison();
                return Err(BucketUpdateRefusal::ClosedBucketReopened);
            }
        }
        let Some(next_revision) = self.revision.checked_add(1) else {
            self.poison();
            return Err(BucketUpdateRefusal::BoardRevisionExhausted);
        };
        if let Some(old) = previous {
            if let Some(rank_key) = RankKey::for_row(&old, metric) {
                self.ordered.remove(&rank_key);
            }
            self.observation_times
                .remove(&(old.last_observed_secs, key));
            self.uncertain -= usize::from(!old.quality.is_eligible() || old.volume_quality != 0);
            self.unavailable_net -= usize::from(old.estimated_net_volume.is_none());
            self.closed -= usize::from(old.closed);
        }
        if let Some(rank_key) = RankKey::for_row(&row, metric) {
            self.ordered.insert(rank_key);
        }
        self.observation_times.insert((row.last_observed_secs, key));
        self.uncertain += usize::from(!row.quality.is_eligible() || row.volume_quality != 0);
        self.unavailable_net += usize::from(row.estimated_net_volume.is_none());
        self.closed += usize::from(row.closed);
        self.rows.insert(key, row);
        self.revision = next_revision;
        self.cached_winner = self
            .ordered
            .first()
            .and_then(|rank| self.rows.get(&rank.identity))
            .copied();
        self.oldest_observed_secs = self.observation_times.first().map(|entry| entry.0);
        self.newest_observed_secs = self.observation_times.last().map(|entry| entry.0);
        Ok(BucketApplyOutcome::Applied)
    }

    fn coverage(&self, expected_contracts: usize) -> BucketCoverage {
        BucketCoverage {
            expected_contracts,
            observed_contracts: self.rows.len(),
            eligible_contracts: self.ordered.len(),
            uncertain_contracts: self.uncertain,
            unavailable_net_contracts: self.unavailable_net,
            closed_contracts: self.closed,
            oldest_observed_secs: self.oldest_observed_secs,
            newest_observed_secs: self.newest_observed_secs,
            integrity_fault: self.integrity_fault,
        }
    }
}

#[derive(Default)]
struct CandleBucket {
    families: [FamilyBucket; 2],
}

/// Single-owner mutable index. The registered universe is fixed for this
/// session/version, while volume and exact ranking update incrementally.
pub struct BucketTopVolumeEngine {
    feed: Feed,
    session_day: u32,
    universe_version: u64,
    metric: BucketVolumeMetric,
    contracts: HashMap<ContractKey, BucketContract>,
    family_counts: [usize; 2],
    retain_buckets: usize,
    buckets: [BTreeMap<u32, CandleBucket>; TOP_VOLUME_TF_COUNT],
}

impl BucketTopVolumeEngine {
    /// Construct a new session/metadata generation. This is a cold-path
    /// O(N) registration step, not work repeated for every market tick.
    pub fn new(
        feed: Feed,
        session_day: u32,
        universe_version: u64,
        metric: BucketVolumeMetric,
        contracts: impl IntoIterator<Item = BucketContract>,
        retain_buckets: usize,
    ) -> Result<Self, BucketConfigError> {
        if session_day == 0 || session_day.checked_mul(86_400).is_none() {
            return Err(BucketConfigError::InvalidSession);
        }
        if universe_version == 0 {
            return Err(BucketConfigError::InvalidUniverseVersion);
        }
        if !(1..=MAX_RETAINED_BUCKETS_PER_TIMEFRAME).contains(&retain_buckets) {
            return Err(BucketConfigError::InvalidRetention);
        }
        let mut registered = HashMap::new();
        let mut family_counts = [0_usize; 2];
        for contract in contracts {
            if contract.security_id == 0
                || contract.security_id > i64::MAX as u64
                || contract.underlying_id == 0
                || contract.underlying_id > i64::MAX as u64
            {
                return Err(BucketConfigError::InvalidIdentity);
            }
            if contract.lot_size == 0 {
                return Err(BucketConfigError::ZeroLotSize);
            }
            if contract.instrument_definition_version == 0
                || contract.instrument_definition_version > i64::MAX as u64
            {
                return Err(BucketConfigError::MissingDefinitionVersion);
            }
            let slot = family_slot(contract.family);
            if family_counts[slot] >= MAX_TRACKED_CONTRACTS {
                return Err(BucketConfigError::FamilyAtCapacity);
            }
            if registered.insert(contract.key(), contract).is_some() {
                return Err(BucketConfigError::DuplicateIdentity);
            }
            family_counts[slot] += 1;
        }
        Ok(Self {
            feed,
            session_day,
            universe_version,
            metric,
            contracts: registered,
            family_counts,
            retain_buckets,
            buckets: std::array::from_fn(|_| BTreeMap::new()),
        })
    }

    #[must_use]
    pub const fn metric(&self) -> BucketVolumeMetric {
        self.metric
    }

    #[must_use]
    pub const fn session_day(&self) -> u32 {
        self.session_day
    }

    #[must_use]
    pub const fn universe_version(&self) -> u64 {
        self.universe_version
    }

    #[must_use]
    pub fn family_of(&self, security_id: u64, segment_code: u8) -> Option<OptionFamily> {
        self.contracts
            .get(&(security_id, segment_code))
            .map(|contract| contract.family)
    }

    fn poison_existing_bucket(&mut self, tf: TfIndex, start: u32, family: OptionFamily) {
        let Some(slot) = tf.top_volume_ordinal() else {
            return;
        };
        if let Some(bucket) = self.buckets[slot].get_mut(&start) {
            bucket.families[family_slot(family)].poison();
        }
    }

    fn poison_malformed_bucket_bounds(&mut self, tf: TfIndex, family: OptionFamily) {
        let Some(slot) = tf.top_volume_ordinal() else {
            return;
        };
        // Invalid bounds cannot identify the intended replacement reliably.
        // Revoke every retained board for this timeframe/family, including
        // the latest published one, instead of rounding or trusting the bad
        // start. This touches at most MAX_RETAINED_BUCKETS_PER_TIMEFRAME
        // cached headers (currently 8), never scans instrument rows, and
        // leaves unrelated families and timeframes unchanged.
        for bucket in self.buckets[slot].values_mut() {
            bucket.families[family_slot(family)].poison();
        }
    }

    /// Replace one canonical candle revision. A correction may decrease
    /// volume or change the sign, so the old exact key is removed first.
    /// An expired correction is explicitly refused, never relabelled as a
    /// current-bucket update. Caller must propagate refusals to observability.
    pub fn apply(
        &mut self,
        update: &CandleVolumeUpdate,
    ) -> Result<BucketApplyOutcome, BucketUpdateRefusal> {
        let slot = update
            .tf
            .top_volume_ordinal()
            .ok_or(BucketUpdateRefusal::UnsupportedTimeframe)?;
        if update.feed != self.feed {
            return Err(BucketUpdateRefusal::WrongFeed);
        }
        if update.session_day != self.session_day {
            return Err(BucketUpdateRefusal::WrongSession);
        }
        let key = (update.security_id, update.segment_code);
        let contract = *self
            .contracts
            .get(&key)
            .ok_or(BucketUpdateRefusal::UnknownContract)?;
        if update.bucket_start_secs / 86_400 != self.session_day
            || update.tf.bucket_start(update.bucket_start_secs) != update.bucket_start_secs
            || update
                .tf
                .observation_window_end(update.bucket_start_secs)
                .is_none()
            || update
                .bucket_start_secs
                .checked_add(update.tf.seconds_per_bucket())
                != Some(update.bucket_end_secs)
        {
            self.poison_malformed_bucket_bounds(update.tf, contract.family);
            return Err(BucketUpdateRefusal::InvalidBucket);
        }
        if update.revision == 0 {
            self.poison_existing_bucket(update.tf, update.bucket_start_secs, contract.family);
            return Err(BucketUpdateRefusal::InvalidRevision);
        }
        if update.last_observed_secs < update.bucket_start_secs {
            self.poison_existing_bucket(update.tf, update.bucket_start_secs, contract.family);
            return Err(BucketUpdateRefusal::InvalidObservationTime);
        }
        if !self
            .metric
            .quantity_is_valid(update.gross_volume, update.estimated_net_volume)
        {
            self.poison_existing_bucket(update.tf, update.bucket_start_secs, contract.family);
            return Err(BucketUpdateRefusal::InvalidSignedVolume);
        }
        let family_code = match contract.family {
            OptionFamily::Stock => 1,
            OptionFamily::Index => 2,
        };
        if update.metadata.lot_size != contract.lot_size
            || update.metadata.instrument_definition_version
                != contract.instrument_definition_version
            || update.metadata.underlying_id != contract.underlying_id
            || update.metadata.family_code != family_code
        {
            self.poison_existing_bucket(update.tf, update.bucket_start_secs, contract.family);
            return Err(BucketUpdateRefusal::MetadataMismatch);
        }
        let buckets = &mut self.buckets[slot];
        if !buckets.contains_key(&update.bucket_start_secs)
            && buckets.len() >= self.retain_buckets
            && buckets
                .first_key_value()
                .is_some_and(|(&oldest, _)| update.bucket_start_secs < oldest)
        {
            return Err(BucketUpdateRefusal::ExpiredBucket);
        }
        let bucket = buckets.entry(update.bucket_start_secs).or_default();
        let result = bucket.families[family_slot(contract.family)].apply(
            BucketRankedContract {
                security_id: contract.security_id,
                segment: contract.segment,
                underlying_id: contract.underlying_id,
                lot_size: contract.lot_size,
                instrument_definition_version: contract.instrument_definition_version,
                gross_volume: update.gross_volume,
                estimated_net_volume: update.estimated_net_volume,
                revision: update.revision,
                last_observed_secs: update.last_observed_secs,
                quality: update.quality,
                volume_quality: update.volume_quality,
                closed: update.closed,
            },
            self.metric,
        );
        while buckets.len() > self.retain_buckets {
            // Reclamation is O(rows in the evicted bucket), explicitly
            // outside any claim of constant-work ordinary updates.
            buckets.pop_first();
        }
        result
    }

    /// Materialize one retained exact bucket, including an explicit empty
    /// family. An older correction can be persisted without replacing the
    /// latest live slot. This is O(rows), not a per-winner selection scan.
    #[must_use]
    pub fn snapshot(
        &self,
        family: OptionFamily,
        tf: TfIndex,
        bucket_start_secs: u32,
        published_secs: u32,
    ) -> Option<Arc<BucketTopVolumeSnapshot>> {
        let bucket = self.buckets[tf.top_volume_ordinal()?].get(&bucket_start_secs)?;
        let state = &bucket.families[family_slot(family)];
        Some(Arc::new(BucketTopVolumeSnapshot {
            feed: self.feed,
            family,
            tf,
            session_day: self.session_day,
            universe_version: self.universe_version,
            metric: self.metric,
            bucket_start_secs,
            bucket_end_secs: tf.bucket_end(bucket_start_secs),
            revision: state.revision,
            published_secs,
            coverage: state.coverage(self.family_counts[family_slot(family)]),
            rows: state
                .ordered
                .iter()
                .filter_map(|key| state.rows.get(&key.identity).copied())
                .collect(),
        }))
    }

    #[must_use]
    pub fn latest_bucket_start(&self, tf: TfIndex) -> Option<u32> {
        self.buckets[tf.top_volume_ordinal()?]
            .last_key_value()
            .map(|(&start, _)| start)
    }

    /// Fixed-size copy of the already indexed winner and coverage. Lookup
    /// costs O(log B), where B is the explicit retained-bucket bound; no
    /// instrument enumeration, ranking or complete-board allocation occurs.
    #[must_use]
    pub fn winner_snapshot(
        &self,
        family: OptionFamily,
        tf: TfIndex,
        bucket_start_secs: u32,
        published_secs: u32,
    ) -> Option<Arc<BucketWinnerSnapshot>> {
        let bucket = self.buckets[tf.top_volume_ordinal()?].get(&bucket_start_secs)?;
        let state = &bucket.families[family_slot(family)];
        Some(Arc::new(BucketWinnerSnapshot {
            feed: self.feed,
            family,
            tf,
            session_day: self.session_day,
            universe_version: self.universe_version,
            metric: self.metric,
            bucket_start_secs,
            bucket_end_secs: tf.bucket_end(bucket_start_secs),
            revision: state.revision,
            published_secs,
            coverage: state.coverage(self.family_counts[family_slot(family)]),
            winner: state.cached_winner,
        }))
    }

    /// Publish the fixed-size winner after an update batch. Historical
    /// corrections remain available from `snapshot`/`winner_snapshot` while
    /// the runtime refuses to replace a newer bucket with an older one.
    pub fn publish_winner(
        &self,
        runtime: &BucketTopVolumeRuntime,
        family: OptionFamily,
        tf: TfIndex,
        bucket_start_secs: u32,
        published_secs: u32,
    ) -> Result<Option<Arc<BucketWinnerSnapshot>>, BucketPublishRefusal> {
        let slot = tf
            .top_volume_ordinal()
            .ok_or(BucketPublishRefusal::UnsupportedTimeframe)?;
        let Some(bucket) = self.buckets[slot].get(&bucket_start_secs) else {
            return Ok(None);
        };
        let revision = bucket.families[family_slot(family)].revision;
        let integrity_fault = bucket.families[family_slot(family)].integrity_fault;
        if runtime.load_winner(family, tf).is_some_and(|old| {
            old.feed == self.feed
                && old.session_day == self.session_day
                && old.universe_version == self.universe_version
                && old.metric == self.metric
                && old.bucket_start_secs == bucket_start_secs
                && old.revision == revision
                && old.coverage.integrity_fault == integrity_fault
        }) {
            return Ok(None);
        }
        let Some(snapshot) = self.winner_snapshot(family, tf, bucket_start_secs, published_secs)
        else {
            return Ok(None);
        };
        runtime.publish_winner(Arc::clone(&snapshot))?;
        Ok(Some(snapshot))
    }

    /// Publish every retained exact bucket winner as one bounded window.
    /// A tick may seal B and open B+1 before publication; both remain available
    /// by exact identity. Unchanged revisions reuse their existing Arc and
    /// publication clock. No full-board rows are copied.
    pub fn publish_winner_latest(
        &self,
        runtime: &BucketTopVolumeRuntime,
        family: OptionFamily,
        tf: TfIndex,
        published_secs: u32,
    ) -> Result<Option<Arc<BucketWinnerSnapshot>>, BucketPublishRefusal> {
        let slot = tf
            .top_volume_ordinal()
            .ok_or(BucketPublishRefusal::UnsupportedTimeframe)?;
        let buckets = &self.buckets[slot];
        if buckets.is_empty() {
            return Ok(None);
        }
        let previous = runtime.winners[family_slot(family)][slot].load_full();
        let mut changed = previous.as_ref().is_none_or(|old| old.len != buckets.len());
        let mut window = BucketWinnerWindow::default();
        for (&start, bucket) in buckets {
            let state = &bucket.families[family_slot(family)];
            let unchanged = previous
                .as_ref()
                .and_then(|old| old.at(start))
                .filter(|old| {
                    old.feed == self.feed
                        && old.session_day == self.session_day
                        && old.universe_version == self.universe_version
                        && old.metric == self.metric
                        && old.revision == state.revision
                        && old.coverage.integrity_fault == state.integrity_fault
                });
            let snapshot = if let Some(old) = unchanged {
                Arc::clone(old)
            } else {
                changed = true;
                self.winner_snapshot(family, tf, start, published_secs)
                    .ok_or(BucketPublishRefusal::InvalidSnapshot)?
            };
            window.push(snapshot)?;
        }
        if !changed {
            return Ok(None);
        }
        let latest = Arc::clone(
            window
                .latest()
                .ok_or(BucketPublishRefusal::InvalidSnapshot)?,
        );
        runtime.publish_winner_window(window)?;
        Ok(Some(latest))
    }

    /// Publish one fully materialized revision after the serialized caller
    /// has applied its candle-update batch. No DB/network work is performed.
    pub fn publish_latest(
        &self,
        runtime: &BucketTopVolumeRuntime,
        family: OptionFamily,
        tf: TfIndex,
        published_secs: u32,
    ) -> Result<Option<Arc<BucketTopVolumeSnapshot>>, BucketPublishRefusal> {
        let slot = tf
            .top_volume_ordinal()
            .ok_or(BucketPublishRefusal::UnsupportedTimeframe)?;
        let Some(start) = self.latest_bucket_start(tf) else {
            return Ok(None);
        };
        let state_version = self.buckets[slot].get(&start).map(|bucket| {
            let state = &bucket.families[family_slot(family)];
            (state.revision, state.integrity_fault)
        });
        if runtime.load(family, tf).is_some_and(|old| {
            old.feed == self.feed
                && old.session_day == self.session_day
                && old.universe_version == self.universe_version
                && old.metric == self.metric
                && old.bucket_start_secs == start
                && Some((old.revision, old.coverage.integrity_fault)) == state_version
        }) {
            return Ok(None);
        }
        let Some(snapshot) = self.snapshot(family, tf, start, published_secs) else {
            return Ok(None);
        };
        runtime.publish(Arc::clone(&snapshot))?;
        Ok(Some(snapshot))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BucketPublishRefusal {
    UnsupportedTimeframe,
    InvalidClock,
    InvalidSnapshot,
    OlderEpoch,
    DifferentEpochContract,
    OlderBucket,
    StaleRevision,
}

/// Readers must name the exact expected session, universe and bucket. An
/// arbitrary latest board cannot satisfy a request for a different period.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BucketDecisionRequest {
    pub feed: Feed,
    pub family: OptionFamily,
    pub tf: TfIndex,
    pub session_day: u32,
    pub universe_version: u64,
    pub metric: BucketVolumeMetric,
    pub bucket_start_secs: u32,
    pub now_secs: u32,
    pub max_age_secs: u32,
    /// Require every declared member to have a qualified seal and the
    /// admitted observation-window end to have passed. Nominal bucket identity
    /// is unchanged; administrative early seals cannot qualify with age.
    pub require_closed: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BucketDecisionRefusal {
    UnsupportedTimeframe,
    Unavailable,
    DifferentEpoch,
    DifferentBucket,
    IncompleteUniverse,
    OpenBucket,
    StalePublication,
    StaleInstrument,
    FutureClock,
    Empty,
}

/// Eight bounded winner windows and eight latest full-board slots: the four
/// admitted Top Volume frames, independently published for stock and index
/// options.
/// Publication/reset share one serialized owner; concurrent readers retain
/// immutable revisions. Previously loaded Arcs stay alive, so consumers must
/// also bound how many revisions they retain.
pub struct BucketTopVolumeRuntime {
    slots: [[ArcSwapOption<BucketTopVolumeSnapshot>; TOP_VOLUME_TF_COUNT]; 2],
    winners: [[ArcSwapOption<BucketWinnerWindow>; TOP_VOLUME_TF_COUNT]; 2],
}

impl Default for BucketTopVolumeRuntime {
    fn default() -> Self {
        Self::new()
    }
}

impl BucketTopVolumeRuntime {
    #[must_use]
    pub fn new() -> Self {
        Self {
            slots: std::array::from_fn(|_| std::array::from_fn(|_| ArcSwapOption::empty())),
            winners: std::array::from_fn(|_| std::array::from_fn(|_| ArcSwapOption::empty())),
        }
    }

    fn validate_winner_snapshot(
        snapshot: &BucketWinnerSnapshot,
    ) -> Result<(), BucketPublishRefusal> {
        if !snapshot.tf.is_top_volume() {
            return Err(BucketPublishRefusal::UnsupportedTimeframe);
        }
        if snapshot.published_secs < snapshot.bucket_start_secs
            || snapshot
                .coverage
                .newest_observed_secs
                .is_some_and(|ts| ts > snapshot.published_secs)
        {
            return Err(BucketPublishRefusal::InvalidClock);
        }
        if snapshot.universe_version == 0
            || snapshot.session_day == 0
            || snapshot.bucket_start_secs / 86_400 != snapshot.session_day
            || snapshot.tf.bucket_start(snapshot.bucket_start_secs) != snapshot.bucket_start_secs
            || snapshot
                .tf
                .observation_window_end(snapshot.bucket_start_secs)
                .is_none()
            || snapshot
                .bucket_start_secs
                .checked_add(snapshot.tf.seconds_per_bucket())
                != Some(snapshot.bucket_end_secs)
            || !snapshot.coverage.structurally_valid()
            || snapshot.winner.is_some() != (snapshot.coverage.eligible_contracts != 0)
            || snapshot
                .winner
                .as_ref()
                .is_some_and(|row| RankKey::for_row(row, snapshot.metric).is_none())
        {
            return Err(BucketPublishRefusal::InvalidSnapshot);
        }
        Ok(())
    }

    fn validate_winner_progress(
        snapshot: &BucketWinnerSnapshot,
        previous: &BucketWinnerSnapshot,
        allow_unchanged: bool,
    ) -> Result<(), BucketPublishRefusal> {
        let previous_epoch = (previous.session_day, previous.universe_version);
        let offered_epoch = (snapshot.session_day, snapshot.universe_version);
        if offered_epoch < previous_epoch {
            return Err(BucketPublishRefusal::OlderEpoch);
        }
        if offered_epoch == previous_epoch {
            if snapshot.feed != previous.feed || snapshot.metric != previous.metric {
                return Err(BucketPublishRefusal::DifferentEpochContract);
            }
            if snapshot.bucket_start_secs < previous.bucket_start_secs {
                return Err(BucketPublishRefusal::OlderBucket);
            }
            if snapshot.bucket_start_secs == previous.bucket_start_secs
                && snapshot.revision <= previous.revision
                && !(allow_unchanged && snapshot == previous)
                && !(snapshot.revision == previous.revision
                    && snapshot.coverage.integrity_fault
                    && !previous.coverage.integrity_fault)
            {
                return Err(BucketPublishRefusal::StaleRevision);
            }
        }
        Ok(())
    }

    /// Publish one explicitly selected latest winner. This compatibility entry
    /// point retains one bucket and refuses an older or duplicate publication.
    /// The live engine uses `publish_winner_latest` to atomically publish its
    /// complete bounded correction window instead.
    pub fn publish_winner(
        &self,
        snapshot: Arc<BucketWinnerSnapshot>,
    ) -> Result<(), BucketPublishRefusal> {
        Self::validate_winner_snapshot(&snapshot)?;
        if let Some(previous) = self.load_winner(snapshot.family, snapshot.tf) {
            Self::validate_winner_progress(&snapshot, &previous, false)?;
        }
        let mut window = BucketWinnerWindow::default();
        window.push(snapshot)?;
        self.publish_winner_window(window)
    }

    fn publish_winner_window(
        &self,
        window: BucketWinnerWindow,
    ) -> Result<(), BucketPublishRefusal> {
        let latest = window
            .latest()
            .ok_or(BucketPublishRefusal::InvalidSnapshot)?;
        let ordinal = latest
            .tf
            .top_volume_ordinal()
            .ok_or(BucketPublishRefusal::UnsupportedTimeframe)?;
        let slot = &self.winners[family_slot(latest.family)][ordinal];
        let previous = slot.load_full();
        if let Some(old) = previous.as_ref().and_then(|old| old.latest()) {
            // Historical corrections may change while the latest bucket is
            // unchanged, but a window must never move the latest epoch back.
            Self::validate_winner_progress(latest, old, true)?;
        }
        let mut previous_start = None;
        for snapshot in window.entries.iter().flatten() {
            Self::validate_winner_snapshot(snapshot)?;
            if snapshot.feed != latest.feed
                || snapshot.family != latest.family
                || snapshot.tf != latest.tf
                || snapshot.session_day != latest.session_day
                || snapshot.universe_version != latest.universe_version
                || snapshot.metric != latest.metric
                || previous_start.is_some_and(|start| start >= snapshot.bucket_start_secs)
            {
                return Err(BucketPublishRefusal::InvalidSnapshot);
            }
            if let Some(old) = previous
                .as_ref()
                .and_then(|old| old.at(snapshot.bucket_start_secs))
            {
                Self::validate_winner_progress(snapshot, old, true)?;
            }
            previous_start = Some(snapshot.bucket_start_secs);
        }
        slot.store(Some(Arc::new(window)));
        Ok(())
    }

    #[must_use]
    pub fn load_winner(
        &self,
        family: OptionFamily,
        tf: TfIndex,
    ) -> Option<Arc<BucketWinnerSnapshot>> {
        self.winners[family_slot(family)][tf.top_volume_ordinal()?]
            .load_full()?
            .latest()
            .map(Arc::clone)
    }

    /// Read the requested retained winner without materializing or loading a
    /// full board. At most eight fixed-size entries are inspected. The same
    /// coverage, source freshness, publication freshness and closure gates
    /// apply to the exact retained bucket; a new live bucket is not a fallback.
    pub fn load_winner_for_decision(
        &self,
        request: BucketDecisionRequest,
    ) -> Result<Arc<BucketWinnerSnapshot>, BucketDecisionRefusal> {
        let ordinal = request
            .tf
            .top_volume_ordinal()
            .ok_or(BucketDecisionRefusal::UnsupportedTimeframe)?;
        let window = self.winners[family_slot(request.family)][ordinal]
            .load_full()
            .ok_or(BucketDecisionRefusal::Unavailable)?;
        let latest = window.latest().ok_or(BucketDecisionRefusal::Unavailable)?;
        if latest.feed != request.feed
            || latest.session_day != request.session_day
            || request.now_secs / 86_400 != latest.session_day
            || latest.universe_version != request.universe_version
            || latest.metric != request.metric
        {
            return Err(BucketDecisionRefusal::DifferentEpoch);
        }
        let snapshot = window
            .at(request.bucket_start_secs)
            .map(Arc::clone)
            .ok_or(BucketDecisionRefusal::DifferentBucket)?;
        if !snapshot.coverage.complete_for_declared_universe() {
            return Err(BucketDecisionRefusal::IncompleteUniverse);
        }
        if request.require_closed
            && (!snapshot.coverage.all_closed()
                || snapshot
                    .tf
                    .observation_window_end(snapshot.bucket_start_secs)
                    .is_none_or(|end| request.now_secs < end || snapshot.published_secs < end))
        {
            return Err(BucketDecisionRefusal::OpenBucket);
        }
        let age = request
            .now_secs
            .checked_sub(snapshot.published_secs)
            .ok_or(BucketDecisionRefusal::FutureClock)?;
        if age > request.max_age_secs {
            return Err(BucketDecisionRefusal::StalePublication);
        }
        let oldest = snapshot
            .coverage
            .oldest_observed_secs
            .ok_or(BucketDecisionRefusal::IncompleteUniverse)?;
        let oldest_age = request
            .now_secs
            .checked_sub(oldest)
            .ok_or(BucketDecisionRefusal::FutureClock)?;
        if oldest_age > request.max_age_secs {
            return Err(BucketDecisionRefusal::StaleInstrument);
        }
        if snapshot
            .coverage
            .newest_observed_secs
            .is_some_and(|newest| newest > request.now_secs)
        {
            return Err(BucketDecisionRefusal::FutureClock);
        }
        if snapshot.first().is_none() {
            return Err(BucketDecisionRefusal::Empty);
        }
        Ok(snapshot)
    }

    /// Publish only after a coherent candle-update batch. Equal revisions
    /// are not republished with a fresher timestamp: a timer alone cannot
    /// make stale data fresh. Empty/ineligible newer boards replace old ones.
    /// Terminal integrity failure is the sole allowed equal-revision change;
    /// it revokes eligibility even when the revision counter is exhausted.
    pub fn publish(
        &self,
        snapshot: Arc<BucketTopVolumeSnapshot>,
    ) -> Result<(), BucketPublishRefusal> {
        let ordinal = snapshot
            .tf
            .top_volume_ordinal()
            .ok_or(BucketPublishRefusal::UnsupportedTimeframe)?;
        if snapshot.published_secs < snapshot.bucket_start_secs
            || snapshot
                .coverage
                .newest_observed_secs
                .is_some_and(|ts| ts > snapshot.published_secs)
        {
            return Err(BucketPublishRefusal::InvalidClock);
        }
        if snapshot.universe_version == 0
            || snapshot.session_day == 0
            || snapshot.bucket_start_secs / 86_400 != snapshot.session_day
            || snapshot.tf.bucket_start(snapshot.bucket_start_secs) != snapshot.bucket_start_secs
            || snapshot
                .tf
                .observation_window_end(snapshot.bucket_start_secs)
                .is_none()
            || snapshot
                .bucket_start_secs
                .checked_add(snapshot.tf.seconds_per_bucket())
                != Some(snapshot.bucket_end_secs)
            || snapshot.rows.len() != snapshot.coverage.eligible_contracts
            || !snapshot.coverage.structurally_valid()
            || snapshot
                .rows
                .iter()
                .any(|row| RankKey::for_row(row, snapshot.metric).is_none())
            || snapshot.rows.windows(2).any(|rows| {
                match (
                    RankKey::for_row(&rows[0], snapshot.metric),
                    RankKey::for_row(&rows[1], snapshot.metric),
                ) {
                    (Some(left), Some(right)) => left > right,
                    _ => true,
                }
            })
        {
            return Err(BucketPublishRefusal::InvalidSnapshot);
        }
        let slot = &self.slots[family_slot(snapshot.family)][ordinal];
        if let Some(previous) = slot.load().as_ref() {
            let previous_epoch = (previous.session_day, previous.universe_version);
            let offered_epoch = (snapshot.session_day, snapshot.universe_version);
            if offered_epoch < previous_epoch {
                return Err(BucketPublishRefusal::OlderEpoch);
            }
            if offered_epoch == previous_epoch {
                if snapshot.feed != previous.feed || snapshot.metric != previous.metric {
                    return Err(BucketPublishRefusal::DifferentEpochContract);
                }
                if snapshot.bucket_start_secs < previous.bucket_start_secs {
                    return Err(BucketPublishRefusal::OlderBucket);
                }
                if snapshot.bucket_start_secs == previous.bucket_start_secs
                    && snapshot.revision <= previous.revision
                    && !(snapshot.revision == previous.revision
                        && snapshot.coverage.integrity_fault
                        && !previous.coverage.integrity_fault)
                {
                    return Err(BucketPublishRefusal::StaleRevision);
                }
            }
        }
        slot.store(Some(snapshot));
        Ok(())
    }

    /// Load the exact publication including incomplete/empty diagnostic
    /// states. Trading consumers must use [`Self::load_for_decision`].
    #[must_use]
    pub fn load(&self, family: OptionFamily, tf: TfIndex) -> Option<Arc<BucketTopVolumeSnapshot>> {
        self.slots[family_slot(family)][tf.top_volume_ordinal()?].load_full()
    }

    /// Fixed work in board length: one shared snapshot load, metadata gates,
    /// then `first()`. Clock reads, synchronization and Arc reclamation are
    /// not included in a claim of bounded nanoseconds. This is a data-quality
    /// gate, not an order authorization or profitability claim.
    pub fn load_for_decision(
        &self,
        request: BucketDecisionRequest,
    ) -> Result<Arc<BucketTopVolumeSnapshot>, BucketDecisionRefusal> {
        if !request.tf.is_top_volume() {
            return Err(BucketDecisionRefusal::UnsupportedTimeframe);
        }
        let snapshot = self
            .load(request.family, request.tf)
            .ok_or(BucketDecisionRefusal::Unavailable)?;
        if snapshot.feed != request.feed
            || snapshot.session_day != request.session_day
            || request.now_secs / 86_400 != snapshot.session_day
            || snapshot.universe_version != request.universe_version
            || snapshot.metric != request.metric
        {
            return Err(BucketDecisionRefusal::DifferentEpoch);
        }
        if snapshot.bucket_start_secs != request.bucket_start_secs {
            return Err(BucketDecisionRefusal::DifferentBucket);
        }
        if !snapshot.coverage.complete_for_declared_universe() {
            return Err(BucketDecisionRefusal::IncompleteUniverse);
        }
        if request.require_closed
            && (!snapshot.coverage.all_closed()
                || snapshot
                    .tf
                    .observation_window_end(snapshot.bucket_start_secs)
                    .is_none_or(|end| request.now_secs < end || snapshot.published_secs < end))
        {
            return Err(BucketDecisionRefusal::OpenBucket);
        }
        let age = request
            .now_secs
            .checked_sub(snapshot.published_secs)
            .ok_or(BucketDecisionRefusal::FutureClock)?;
        if age > request.max_age_secs {
            return Err(BucketDecisionRefusal::StalePublication);
        }
        let oldest = snapshot
            .coverage
            .oldest_observed_secs
            .ok_or(BucketDecisionRefusal::IncompleteUniverse)?;
        let oldest_age = request
            .now_secs
            .checked_sub(oldest)
            .ok_or(BucketDecisionRefusal::FutureClock)?;
        if oldest_age > request.max_age_secs {
            return Err(BucketDecisionRefusal::StaleInstrument);
        }
        if snapshot
            .coverage
            .newest_observed_secs
            .is_some_and(|newest| newest > request.now_secs)
        {
            return Err(BucketDecisionRefusal::FutureClock);
        }
        if snapshot.first().is_none() {
            return Err(BucketDecisionRefusal::Empty);
        }
        Ok(snapshot)
    }

    /// Serialized owner only. This is not an atomic multi-slot reset; every
    /// decision request still verifies the expected session/version.
    pub fn reset(&self) {
        for family in &self.slots {
            for slot in family {
                slot.store(None);
            }
        }
        for family in &self.winners {
            for slot in family {
                slot.store(None);
            }
        }
    }
}

pub fn global_bucket_top_volume_runtime() -> &'static BucketTopVolumeRuntime {
    static RUNTIME: OnceLock<BucketTopVolumeRuntime> = OnceLock::new();
    RUNTIME.get_or_init(BucketTopVolumeRuntime::new)
}

#[cfg(test)]
mod tests;
