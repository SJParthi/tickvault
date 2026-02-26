//! Tick pipeline — connects WebSocket frames to parser, storage, and candle aggregator.
//!
//! # Flow
//! `WebSocket Pool → mpsc::Receiver<Vec<u8>> → dispatch_frame → ParsedTick`
//! Then for each tick:
//! 1. `TickPersistenceWriter::append_tick()` — batched QuestDB write
//! 2. `CandleManager::process_tick()` — O(1) candle updates
//! 3. For each finalized candle → `broadcast::Sender::send()` to API clients

pub mod tick_processor;

pub use tick_processor::run_tick_processor;
