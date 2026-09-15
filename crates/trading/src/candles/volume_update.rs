//! Bounded canonical quantity publications shared by candles and ranking.

use tickvault_common::feed::Feed;
use tickvault_common::tick_types::VolumeQuality;

use super::{CandleMetadata, TF_COUNT, TfIndex};

pub const CANDLE_SIGNED_NET_VS_ONE_LOT_METRIC: &str = "signed_estimated_net_vs_one_lot_v2";
/// Whole candle magnitude signed by its close versus the frozen prior same-TF close.
pub const CANDLE_SIGNED_BAR_VS_ONE_LOT_METRIC: &str = "signed_bar_volume_vs_one_lot_v3";

/// Conservative callback capacity: F session seals + F baseline amendments +
/// F fold seals/amendments + F current states, all for this one instrument.
pub const MAX_VOLUME_UPDATES_PER_TICK: usize = 4 * TF_COUNT;

pub const VOLUME_QUALITY_UNKNOWN_METADATA: u32 = 1 << 0;
pub const VOLUME_QUALITY_DEFINITION_CHANGED: u32 = 1 << 1;
pub const VOLUME_QUALITY_UNKNOWN_BASELINE: u32 = 1 << 2;
pub const VOLUME_QUALITY_COUNTER_AMBIGUOUS: u32 = 1 << 3;
pub const VOLUME_QUALITY_MISSING_OBSERVATION: u32 = 1 << 4;
pub const VOLUME_QUALITY_ATTRIBUTION_UNCERTAIN: u32 = 1 << 5;
pub const VOLUME_QUALITY_UNCLASSIFIED_NET: u32 = 1 << 6;
pub const VOLUME_QUALITY_REVISION_EXHAUSTED: u32 = 1 << 7;
/// Persisted signed cache predates the whole-bar volume basis. Never relabel it.
pub const VOLUME_QUALITY_LEGACY_BASIS: u32 = 1 << 9;

#[inline]
#[must_use]
pub const fn volume_quality_bits(quality: VolumeQuality) -> u32 {
    (if quality.baseline_known {
        0
    } else {
        VOLUME_QUALITY_UNKNOWN_BASELINE
    }) | (if quality.counter_ambiguous {
        VOLUME_QUALITY_COUNTER_AMBIGUOUS
    } else {
        0
    }) | (if quality.volume_missing {
        VOLUME_QUALITY_MISSING_OBSERVATION
    } else {
        0
    }) | (if quality.attribution_uncertain {
        VOLUME_QUALITY_ATTRIBUTION_UNCERTAIN
    } else {
        0
    })
}

#[inline]
#[must_use]
pub const fn volume_quality_from_bits(bits: u32) -> VolumeQuality {
    VolumeQuality {
        baseline_known: bits & VOLUME_QUALITY_UNKNOWN_BASELINE == 0,
        counter_ambiguous: bits & VOLUME_QUALITY_COUNTER_AMBIGUOUS != 0,
        volume_missing: bits & VOLUME_QUALITY_MISSING_OBSERVATION != 0,
        attribution_uncertain: bits & VOLUME_QUALITY_ATTRIBUTION_UNCERTAIN != 0,
    }
}

/// One candle state, published directly from the fold rather than recomputed
/// by a ranking timer. The current signed value uses the whole-bar magnitude
/// and frozen previous same-timeframe close. `None` means unavailable; it is
/// distinct from an observed equal-close bar with a known signed value of zero.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CandleVolumeUpdate {
    pub feed: Feed,
    pub security_id: u64,
    pub segment_code: u8,
    pub tf: TfIndex,
    /// IST day ordinal under the existing candle timestamp convention.
    pub session_day: u32,
    pub bucket_start_secs: u32,
    pub bucket_end_secs: u32,
    pub gross_volume: u64,
    /// Whole-bar signed cache; legacy name retained for transport compatibility.
    pub estimated_net_volume: Option<i64>,
    /// Instrument definition pinned when this bucket first opened.
    pub metadata: CandleMetadata,
    /// Persisted flags; zero means locally eligible under the observed-data contract.
    pub volume_quality: u32,
    /// Monotonic per instrument instance, including amendments and seals.
    pub revision: u64,
    /// Accepted observation's bounded receipt clock in IST wall-clock seconds,
    /// with the existing source-time fallback when receipt is unavailable or
    /// implausible. Separate from the bucket clock and never publication time.
    pub last_observed_secs: u32,
    pub quality: VolumeQuality,
    /// The admitted observation window has expired and the local candle state
    /// was sealed under that cutoff. Administrative early flushes are false.
    /// This is not delivery finality; late amendments remain possible.
    pub closed: bool,
}
