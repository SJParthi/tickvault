//! In-memory candle aggregation engines for the 29-timeframes plan
//! (Phase 3). Trading bot reads OHLCV from RAM via `CandleEngine`
//! instead of polling QuestDB matviews.
//!
//! Per plan §2 (CASCADE design): only the `1s` engine sits on the
//! per-tick hot path. All 28 derived engines (`3s/5s/.../1mo`) are
//! fed by sealed `1s` bars over a background SPSC channel.
//!
//! ## Module map
//!
//! - `engine` — `CandleEngine<TF>`, `Bar`, `Timeframe` trait, 7
//!   concrete TF marker types (`Tf1s`, `Tf5s`, ..., `Tf15m`). The
//!   foundation primitive used by both the 1s hot-path engine and
//!   the cascade-driven derived engines.
//!
//! ## Production wiring (NOT shipped here)
//!
//! Wiring `CandleEngine<Tf1s>` into `tick_processor.rs` SPSC consumer
//! and adding the cascade plumbing for the 28 derived engines is
//! deferred to Phase 3 commits 2-3 (per plan §6). Phase 3 commit 4
//! (parity tests against live `candles_*` matviews) is gated on the
//! 7-day soak per plan §6.

pub mod aggregator_cell;
pub mod cascade;
pub mod cascade_fanout;
pub mod engine;
pub mod engine_map;
pub mod multi_tf_aggregator;
pub mod parity;
pub mod pct_stamping;
pub mod tf_index;

pub use aggregator_cell::{AggregatorCell, ConsumeOutcome, LiveCandleState};
pub use cascade::{
    run_cascade_1s, run_midnight_rollover_task_with_fanout, spawn_supervised_cascade_1s,
};
pub use cascade_fanout::{CascadeFanout, DERIVED_ENGINE_COUNT};
pub use engine::{
    Bar, CandleEngine, Tf1d, Tf1h, Tf1m, Tf1mo, Tf1s, Tf1w, Tf2h, Tf3h, Tf3s, Tf4h, Tf5m, Tf5s,
    Tf10s, Tf15m, Tf15s, Tf30m, Tf30s, Timeframe,
};
pub use engine_map::CandleEngineMap;
pub use multi_tf_aggregator::{ConsumeStats, InstrumentEntry, MultiTfAggregator};
pub use parity::{InstrumentKey, ParityMismatch, ParityReport, compare_bars, sweep_parity};
pub use pct_stamping::{
    PrevDayRefs, compute_close_pct, compute_oi_pct, compute_volume_pct, stamp_bar_pct_fields,
};
pub use tf_index::TfIndex;
