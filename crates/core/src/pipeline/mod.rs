//! Tick pipeline — pure capture: receives raw binary frames from WebSocket,
//! parses them, filters junk ticks, and persists to QuestDB via ILP.
//!
//! # Flow
//! `WebSocket Pool → mpsc::Receiver<Bytes> → dispatch_frame → ParsedTick`
//! → junk filter (LTP > 0, valid timestamp) → `TickPersistenceWriter` → QuestDB
//! → `CandleAggregator` → 1s OHLCV candles
//! → `TopMoversTracker` → ranked gainers/losers/most-active snapshots

pub mod boot_ordering_gate;
pub mod candle_aggregator;
pub mod depth_sequence_tracker;
pub mod first_seen_set;
pub mod mover_classifier;
pub mod movers_window;
pub mod no_tick_watchdog;
pub mod option_movers;
// `preopen_movers` retired 2026-05-03 — its functionality (phase=PREOPEN
// rows during 09:00-09:13 IST) was folded into the canonical
// `movers_1s.phase` SYMBOL column populated by `movers_pipeline`.
// The legacy `stock_movers` table it wrote into is dropped by the
// one-shot migration in `movers_base_persistence`.
pub mod last_seen_ltt_cache;
pub mod prev_close_persist;
pub mod prev_close_writer;
pub mod prev_day_close_stamper;
pub mod prev_oi_cache;
pub mod tick_enricher;
pub mod tick_gap_detector;
pub mod tick_processor;
pub mod top_movers;
pub mod volume_delta_tracker;
pub mod volume_monotonicity_guard;

pub use candle_aggregator::CandleAggregator;
pub use depth_sequence_tracker::{DepthSequenceTracker, SequenceOutcome};
pub use option_movers::{OptionMoversTracker, SharedOptionMoversSnapshot};
// `preopen_movers` re-export retired 2026-05-03 along with the module —
// `phase=PREOPEN` rows now live in `movers_1s.phase`.
pub use tick_gap_detector::{
    SharedTickGapDetector, TICK_GAP_COALESCE_WINDOW_SECS_DEFAULT, TICK_GAP_THRESHOLD_SECS_DEFAULT,
    TickGapDetector, TickGapKey,
};
pub use tick_processor::{init_prev_close_cache_dir, run_tick_processor};
pub use top_movers::{SharedTopMoversSnapshot, TopMoversTracker};
