//! In-memory candle aggregation — Engine B (the only candle engine).
//!
//! Candle-engine re-architecture #T1b: Engine A (the legacy 1s
//! `candle_aggregator` → `candles_1s`) and Engine C (the
//! `CandleEngine` / `CandleEngineMap` / `CascadeFanout` matview
//! cascade) were DELETED. Engine B — the per-instrument 21-timeframe
//! aggregator — flushes sealed candles directly to 21 plain
//! `candles_<tf>` QuestDB tables via the storage seal-writer chain.
//!
//! ## Module map
//!
//! - `tf_index` — `TfIndex` (21-TF enum) + table-name / dedup-key
//!   derivation, the single source of truth for TF identity.
//! - `aggregator_cell` — `AggregatorCell` (per-instrument 21-slot
//!   state) + `LiveCandleState` + `ConsumeOutcome`.
//! - `multi_tf_aggregator` — `MultiTfAggregator` (multi-instrument
//!   container) keyed on `(security_id, exchange_segment)`.
//! - `seal_ring` — `SealRing` + `BufferedSeal` ring buffer.
//! - `pct_stamping` — seal-time prev-day pct stamping primitives.
//! - `heartbeat` — per-minute aggregator heartbeat counters.
//! - `boundary_calc` — cold-path boundary-timer pure-function
//!   primitives (IST-midnight math, missed-boundary detection).

pub mod aggregator_cell;
pub mod boundary_calc;
pub mod heartbeat;
pub mod multi_tf_aggregator;
pub mod pct_stamping;
pub mod seal_ring;
pub mod tf_index;

pub use aggregator_cell::{
    AggregatorCell, ConsumeOutcome, FeedStrategy, LatePolicy, LiveCandleState,
};
pub use heartbeat::{AggregatorHeartbeatCounters, AggregatorHeartbeatSnapshot};
pub use multi_tf_aggregator::{
    CATCHUP_SEAL_LATENESS_MARGIN_SECS, CATCHUP_SEAL_POLL_INTERVAL_SECS, ConsumeStats,
    InstrumentEntry, MultiTfAggregator,
};
pub use pct_stamping::{
    PrevDayRefs, compute_close_pct, compute_oi_pct, compute_volume_pct, stamp_seal_pct_fields,
};
pub use seal_ring::{BufferOutcome, BufferedSeal, SEAL_BUFFER_CAPACITY, SealRing};
pub use tf_index::{TF_COUNT, TfIndex};
