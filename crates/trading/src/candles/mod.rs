//! In-memory candle aggregation — Engine B (the only candle engine).
//!
//! Candle-engine re-architecture #T1b: Engine A (the legacy 1s
//! `candle_aggregator` → `candles_1s`) and Engine C (the
//! `CandleEngine` / `CandleEngineMap` / `CascadeFanout` matview
//! cascade) were DELETED. Engine B's SEAL side — [`BufferedSeal`] +
//! [`SealRing`] + the storage seal-writer chain — flushes sealed candles
//! directly to 21 plain `candles_<tf>` QuestDB tables.
//!
//! ## 2026-07-17 (stage-3 dead-WS sweep) — the TICK aggregator is DELETED
//!
//! With both live feeds retired (Dhan 2026-07-13, Groww 2026-07-15) the
//! publisher-less 21-TF TICK aggregator died: `aggregator_cell`
//! (`AggregatorCell`/`ConsumeOutcome`/`FeedStrategy`/`LatePolicy`),
//! `multi_tf_aggregator` (`MultiTfAggregator`, the watermark catch-up
//! seal + `CATCHUP_*` constants), and `heartbeat`
//! (`AggregatorHeartbeatCounters`) are all deleted — they had NO tick
//! input on the REST-only runtime. The sole surviving seal PRODUCER is
//! the REST-era bar fold (`crates/app/src/rest_candle_fold.rs`,
//! FOLD-01), which constructs [`LiveCandleState`] literally from
//! official `spot_1m_rest` bars and emits [`BufferedSeal`]s into the
//! shared seal-writer channel.
//!
//! ## Module map
//!
//! - `tf_index` — `TfIndex` (21-TF enum) + table-name / dedup-key
//!   derivation, the single source of truth for TF identity.
//! - `live_candle_state` — [`LiveCandleState`], the shared per-bucket
//!   OHLCV state (extracted from the deleted `aggregator_cell`).
//! - `seal_ring` — `SealRing` + `BufferedSeal` ring buffer.
//! - `pct_stamping` — DELETED (dead-code cleanup — BATCH-5): the
//!   seal-time prev-day pct-stamping primitives lost their sole feeder
//!   (the deleted `PrevDayCache` boot loader) with the live-WS feed
//!   retirements; the REST-era candle fold (`rest_candle_fold.rs`) is
//!   the sole `candles_*` writer and stamps no pct fields.
//! - `boundary_calc` — DELETED (dead live-WS sweep stage 1, 2026-07-17,
//!   operator directive via coordinator): the cold-path boundary-timer
//!   pure-function primitives had zero callers anywhere.

//! ## 2026-08-09 — the TICK aggregator is REBUILT
//!
//! The operator's dated 2026-08-09 authorization revives the Dhan live
//! main-feed WebSocket (`websocket-connection-scope-lock.md`, "2026-08-09 —
//! DHAN LIVE MAIN-FEED WS REVIVAL AUTHORIZED"), which restores a tick source
//! and therefore the need for a tick→timeframe fold. `aggregator_cell` and
//! `multi_tf_aggregator` are back — REBUILT, not restored: single-owner
//! (`&mut self`, no per-slot `Mutex`, no `papaya`), keyed on the FULL
//! composite `(feed, security_id, exchange_segment_code)`, with a bounded
//! fail-closed slot table and a fail-closed price guard at ingest. See
//! `aggregator_cell`'s module docs for the full diff against the deleted
//! shape, and `multi_tf_aggregator`'s for the key + slot-allocation rationale.
//!
//! The REST-era bar fold (`crates/app/src/rest_candle_fold.rs`, FOLD-01)
//! remains a separate, independent seal producer — the two write the same
//! tables and are distinguished by the `feed` column that is part of every
//! candle DEDUP key.

pub mod aggregator_cell;
mod fold_counters;
pub use fold_counters::seed_fold_counter_baselines;
pub mod live_candle_state;
pub mod multi_tf_aggregator;
pub mod seal_ring;
pub mod tf_index;

pub use aggregator_cell::{
    AggregatorCell, ConsumeOutcome, FeedStrategy, LatePolicy, tick_price_is_sane,
};
pub use live_candle_state::LiveCandleState;
pub use multi_tf_aggregator::{AGGREGATOR_MAX_SLOTS, ConsumeStats, MultiTfAggregator};
pub use seal_ring::{BufferOutcome, BufferedSeal, SEAL_BUFFER_CAPACITY, SealRing};
pub use tf_index::{TF_COUNT, TfIndex};
