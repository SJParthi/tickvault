//! In-memory RAM stores for the REST-era runtime.
//!
//! ## Movers retirement
//!
//! The `top_n` module (`top_n_by_bars` / `TopNQuery` / `Category` /
//! `Scope`) was deleted in PR #2 of the AWS-lifecycle 14-PR sequence
//! alongside the movers pipeline (operator-locked 2026-05-18). Under
//! the 4-IDX_I-only universe, top-N queries return meaningless results.
//!
//! ## What this module ships (REST-era)
//!
//! - [`day_ohlc_tracker::DayOhlcTracker`] — per-index day OHLC folded
//!   from the REST per-minute spot legs (no live tick stream).
//! - [`spot_bar_store`] — month-deep in-RAM spot bar rings.

// Dead live-WS sweep stage 1 (2026-07-17, operator directive via
// coordinator): `bar_cache` (in-RAM bar cache — its only writer was the
// deleted feed-blind `bar_cache_loader` boot union, feed-separation recon
// GAP-1) and `pct_change_cache` (its only documented caller was the dead
// `tick_processor::on_tick`) DELETED — zero production callers of either.
//
// ⚠ RETIRED 2026-07-19 (dead-code cleanup — BATCH-5): the `tick_storage`
// ring (`TickStorage`), its per-day `reset_scheduler`, its broadcast
// `consumer` (`run_tick_storage_consumer`), and the boot-time
// `prev_day_cache` (`PrevDayCache`) + its seal-time `pct_stamping` were
// DELETED. The tick pipeline lost its sole producer with the live-WS feed
// retirements (Dhan 2026-07-13, Groww 2026-07-15) — the tick broadcast has
// ZERO `.send()` producers, so the consumer/ring/reset never ran, and the
// `PrevDayCache` boot loader (PREVCLOSE-04) fed a seal-time pct-stamp path
// that no longer exists in the REST-era candle fold (`rest_candle_fold.rs`
// is now the sole `candles_*` writer). Live modules kept: `day_ohlc_tracker`
// (index day-OHLC from the REST spot legs) + `spot_bar_store` (month-deep
// RAM spot bars).
pub mod day_ohlc_tracker;
pub mod spot_bar_store;

pub use day_ohlc_tracker::{DayOhlc, DayOhlcTracker};
