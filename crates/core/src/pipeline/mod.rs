//! Tick pipeline — pure capture: receives raw binary frames from WebSocket,
//! parses them, filters junk ticks, and persists to QuestDB via ILP.
//!
//! # Flow
//! `WebSocket Pool → mpsc::Receiver<Bytes> → dispatch_frame → ParsedTick`
//! → junk filter (LTP > 0, valid timestamp) → `TickPersistenceWriter` → QuestDB
//!
//! Candle aggregation is a SEPARATE concern handled off this hot path:
//! the multi-TF aggregator (Engine B) subscribes to the tick broadcast.
//! Candle-engine re-architecture #T1b deleted the legacy
//! `candle_aggregator` module (Engine A — `candles_1s`).
//!
//! ## Movers retirement
//!
//! The `top_movers` / `option_movers` / `mover_classifier` / `movers_window`
//! modules were deleted in PR #2 of the AWS-lifecycle 14-PR sequence
//! (operator-locked 2026-05-18). Under the 4-IDX_I-only subscription
//! scope (NIFTY/BANKNIFTY/SENSEX/INDIA VIX) ranked gainers/losers/most-
//! active snapshots are meaningless — there are only 4 instruments.

pub mod boot_ordering_gate;
pub mod chain_day_store;
pub mod chain_snapshot;
// Candle-engine re-architecture #T1b: `candle_aggregator` (Engine A)
// DELETED — Engine B (the multi-TF aggregator) is the only candle engine.
// PR #4 (2026-05-19): `depth_sequence_tracker` module DELETED.
pub mod feed_consumer;
pub mod feed_lag_monitor;
pub mod feed_presence;
pub mod first_seen_set;
pub mod prev_close_writer;
pub mod prev_day_close_stamper;
pub mod prev_oi_cache;
pub mod tick_enricher;
// tick_gap_detector DELETED in PR-C3 (2026-07-14, operator Q4-ii 2026-07-13
// — websocket-connection-scope-lock.md "2026-07-13 Amendment" §B item 4):
// fed only by the retired Dhan WS pipeline; WS-GAP-06 retired with it.
pub mod tick_processor;
pub mod volume_delta_tracker;
pub mod volume_monotonicity_guard;

// Candle-engine re-architecture #T1b: `CandleAggregator` re-export retired.
// PR #4 (2026-05-19): depth_sequence_tracker re-exports retired.
// PR-C3 (2026-07-14): tick_gap_detector re-exports retired with the module.
pub use tick_processor::{init_prev_close_cache_dir, run_tick_processor};
