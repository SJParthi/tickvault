//! Wave-5 in-memory store — Plan §K-L9 / L10 / L11.
//!
//! Ships the in-RAM **tick ring per instrument** that downstream
//! consumers (depth-dynamic selector, top-N movers query, future
//! `/api/movers/v2` read flip in #505) need to bypass QuestDB on
//! the hot read path.
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
//! - The `/api/movers/v2` read flip (#505).
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

pub mod consumer;
pub mod reset_scheduler;
pub mod tick_storage;

pub use consumer::run_tick_storage_consumer;
pub use reset_scheduler::{run_tick_storage_daily_reset, secs_until_next_market_open_ist};
pub use tick_storage::{DEFAULT_PER_INSTRUMENT_CAPACITY, TickStorage};
