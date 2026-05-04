//! Tick pipeline — pure capture: receives raw binary frames from WebSocket,
//! parses them, filters junk ticks, and persists to QuestDB via ILP.
//!
//! # Flow
//! `WebSocket Pool → mpsc::Receiver<Bytes> → dispatch_frame → ParsedTick`
//! → junk filter (LTP > 0, valid timestamp) → `TickPersistenceWriter` → QuestDB
//! → `CandleAggregator` → 1s OHLCV candles
//!
//! PR #457 (2026-05-04): the legacy `top_movers`, `option_movers`, and
//! `movers_window` submodules are gone. The unified `/api/movers`
//! endpoint reads the `movers_5s` materialised view directly (PR #450);
//! the per-tick in-memory tracker is no longer in the hot loop.

pub mod candle_aggregator;
pub mod depth_sequence_tracker;
pub mod first_seen_set;
pub mod mover_classifier;
// PR #457: `movers_window` deleted (legacy ring-buffer for in-memory tracker).
pub mod no_tick_watchdog;
// PR #457: `option_movers` deleted (legacy in-memory `OptionMoversTracker`).
// `preopen_movers` retired 2026-05-03 — phase=PREOPEN rows now live in
// `movers_1s.phase` (populated by `movers_pipeline`).
pub mod prev_close_persist;
pub mod prev_close_writer;
pub mod tick_gap_detector;
pub mod tick_processor;
// PR #457: `top_movers` deleted (legacy in-memory `TopMoversTracker` +
// `MoversTrackerV2` + `SharedTopMoversSnapshot`).
pub mod volume_monotonicity_guard;

pub use candle_aggregator::CandleAggregator;
pub use depth_sequence_tracker::{DepthSequenceTracker, SequenceOutcome};
// PR #457: `OptionMoversTracker` + `SharedOptionMoversSnapshot` re-exports
// removed alongside the module.
pub use tick_gap_detector::{
    SharedTickGapDetector, TICK_GAP_COALESCE_WINDOW_SECS_DEFAULT, TICK_GAP_THRESHOLD_SECS_DEFAULT,
    TickGapDetector, TickGapKey,
};
pub use tick_processor::{init_prev_close_cache_dir, run_tick_processor};
// PR #457: `TopMoversTracker` + `SharedTopMoversSnapshot` re-exports removed.
