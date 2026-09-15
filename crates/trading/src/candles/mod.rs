//! In-memory candle aggregation and the shared canonical seal contract.
//!
//! The live engine maintains exactly ten active timeframes: 1s, 3s, 5s,
//! 1m, 3m, 5m, 10m, 15m, 30m and 60m. [`TfIndex`] provides dense runtime
//! indices, while its separate storage IDs preserve old spill identities.
//! Retired frames have no live aggregation or writer slots.
//!
//! [`MultiTfAggregator`] owns the bounded per-instrument candle state.
//! [`CandleVolumeUpdate`] carries the same canonical quantities into the
//! in-memory rankings for only 1s, 3s, 5s and 1m. [`BufferedSeal`] carries
//! completed or amended candles for all ten frames through [`SealRing`] to
//! the storage writer. The feed remains part of the persisted candle identity.
//!
//! [`regular_observation_window_end`] names the existing local capture
//! close independently of the active timeframe list. It does not replace
//! lateness, queue-drain, freshness or provider-completeness checks.

pub mod aggregator_cell;
mod fold_counters;
pub use fold_counters::seed_fold_counter_baselines;
pub mod live_candle_state;
pub mod multi_tf_aggregator;
pub mod seal_ring;
pub mod tf_index;
pub mod volume_update;

pub use aggregator_cell::{
    AggregatorCell, ConsumeOutcome, FeedStrategy, LatePolicy, tick_price_is_sane,
};
pub use live_candle_state::{CandleMetadata, LiveCandleState};
pub use multi_tf_aggregator::{AGGREGATOR_MAX_SLOTS, ConsumeStats, MultiTfAggregator};
pub use seal_ring::{BufferOutcome, BufferedSeal, SEAL_BUFFER_CAPACITY, SealRing};
pub use tf_index::{
    CANDLE_OBSERVATION_WINDOW_POLICY, TF_COUNT, TOP_VOLUME_TF_COUNT, TfIndex,
    regular_observation_window_end,
};
pub use volume_update::{
    CANDLE_SIGNED_BAR_VS_ONE_LOT_METRIC, CANDLE_SIGNED_NET_VS_ONE_LOT_METRIC, CandleVolumeUpdate,
};
