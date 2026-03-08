//! Tick pipeline — pure capture: receives raw binary frames from WebSocket,
//! parses them, filters junk ticks, and persists to QuestDB via ILP.
//!
//! # Flow
//! `WebSocket Pool → mpsc::Receiver<Bytes> → dispatch_frame → ParsedTick`
//! → junk filter (LTP > 0, valid timestamp) → `TickPersistenceWriter` → QuestDB
//! → `CandleAggregator` → 1s OHLCV candles
//! → `TopMoversTracker` → ranked gainers/losers/most-active snapshots

pub mod candle_aggregator;
pub mod tick_processor;
pub mod top_movers;

pub use candle_aggregator::CandleAggregator;
pub use tick_processor::run_tick_processor;
pub use top_movers::TopMoversTracker;
