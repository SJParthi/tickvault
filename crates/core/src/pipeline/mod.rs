//! Tick pipeline — pure capture: receives raw binary frames from WebSocket,
//! parses them, filters junk ticks, and persists to QuestDB via ILP.
//!
//! # Flow
//! `WebSocket Pool → mpsc::Receiver<Bytes> → dispatch_frame → ParsedTick`
//! → junk filter (LTP > 0, valid timestamp) → `TickPersistenceWriter` → QuestDB
//! → `CandleAggregator` → 1s OHLCV candles
//! → `TopMoversTracker` → ranked gainers/losers/most-active snapshots

pub mod candle_aggregator;
pub mod depth_sequence_tracker;
pub mod first_seen_set;
pub mod mover_classifier;
pub mod movers_window;
pub mod no_tick_watchdog;
pub mod option_movers;
pub mod prev_close_writer;
pub mod tick_processor;
pub mod top_movers;

pub use candle_aggregator::CandleAggregator;
pub use depth_sequence_tracker::{DepthSequenceTracker, SequenceOutcome};
pub use option_movers::{OptionMoversTracker, SharedOptionMoversSnapshot};
pub use tick_processor::{init_prev_close_cache_dir, run_tick_processor};
pub use top_movers::{SharedTopMoversSnapshot, TopMoversTracker};
