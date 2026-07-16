//! Wave-5 in-memory store — Plan §K-L9 / L10 / L11.
//!
//! Ships the in-RAM **tick ring per instrument** that downstream
//! consumers (depth-dynamic selector, etc.) need to bypass QuestDB on
//! the hot read path.
//!
//! ## Movers retirement
//!
//! The `top_n` module (`top_n_by_bars` / `TopNQuery` / `Category` /
//! `Scope`) was deleted in PR #2 of the AWS-lifecycle 14-PR sequence
//! alongside the movers pipeline (operator-locked 2026-05-18). Under
//! the 4-IDX_I-only universe, top-N queries return meaningless results.
//!
//! ## What this module ships (PR #504d)
//!
//! - [`tick_storage::TickStorage`] — `papaya<(security_id, exchange_segment),
//!   Arc<Mutex<Vec<ParsedTick>>>>` keyed full-day tick history.
//! - [`reset_scheduler`] — IST 09:15 daily reset task that drains the
//!   storage so day-N ticks never bleed into day-(N+1).
//!
//! ## What this module does NOT ship (deliberate scope)
//!
//! - The bar storage map (`CascadeFanout` already exists from
//!   Phase 3 / #504c — L11's "bar storage" requirement is met by it).
//! - The seal-time % stamping (#504e — populates the 5 fields shipped
//!   in #504b).
//! - The 1s engine retirement per L17 (separate cleanup PR).
//!
//! ## Hot-path budget
//!
//! Per plan L12: ≤200 ns per tick total budget (across all hot-path
//! work). [`TickStorage::push`] adds ~80 ns to that budget:
//! - papaya pin/get: ~30 ns
//! - `Mutex::lock` (uncontended): ~5 ns
//! - `Vec::push` amortised O(1): ~20 ns when capacity sufficient
//! - lock release: ~5 ns
//!
//! The `Vec::push` realloc cost is amortised; with the runtime-tunable
//! `[in_mem.tick_storage].per_instrument_capacity` boot-time hint,
//! steady-state ticks hit the no-realloc path.
//!
//! ## Reset semantics
//!
//! At IST 09:15:00 daily the reset task drains every per-instrument
//! `Vec<ParsedTick>` (clears + retains the allocated capacity so the
//! next day's first push does NOT re-allocate). Drain is `O(N)` over
//! the keys; happens once per trading day off the hot path.

pub mod bar_cache;
pub mod consumer;
pub mod day_ohlc_tracker;
pub mod pct_change_cache;
pub mod prev_day_cache;
pub mod reset_scheduler;
pub mod spot_bar_store;
pub mod tick_storage;

pub use bar_cache::{BarCache, CompactBar, bar_cache_clear_before_threshold};
pub use consumer::run_tick_storage_consumer;
pub use day_ohlc_tracker::{DayOhlc, DayOhlcTracker};
pub use pct_change_cache::{PctChange, PctChangeCache};
pub use prev_day_cache::PrevDayCache;
pub use reset_scheduler::{run_tick_storage_daily_reset, secs_until_next_market_open_ist};
pub use tick_storage::{DEFAULT_PER_INSTRUMENT_CAPACITY, TickStorage};
