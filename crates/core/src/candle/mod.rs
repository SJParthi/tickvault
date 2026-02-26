//! Candle aggregator — O(1) per-tick OHLCV candle generation.
//!
//! Supports both time-based intervals (IST-aligned boundaries) and
//! tick-count-based intervals. All 27 standard TradingView timeframes
//! plus custom time/tick intervals are supported.
//!
//! # Architecture
//! - `RollingCandleState` — O(1) state machine for a single (security_id, interval) pair
//! - `TickCandleState` — O(1) state machine for tick-count-based candles
//! - `CandleManager` — manages all active candle states, returns finalized candles

pub mod candle_manager;
pub mod rolling_candle;

pub use candle_manager::CandleManager;
pub use rolling_candle::{RollingCandleState, TickCandleState};
