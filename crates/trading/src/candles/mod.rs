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

pub mod cascade;
pub mod engine;
pub mod engine_map;

pub use cascade::{run_cascade_1s, run_midnight_rollover_task, spawn_supervised_cascade_1s};
pub use engine::{Bar, CandleEngine, Tf1m, Tf1s, Tf5m, Tf5s, Tf15m, Tf15s, Tf30s, Timeframe};
pub use engine_map::CandleEngineMap;
