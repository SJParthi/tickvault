//! Master instrument download, parsing, and F&O universe building.
//!
//! Downloads Dhan's instrument master CSV daily, parses it, runs a 5-pass
//! mapping algorithm, and produces a complete `FnoUniverse` with all lookup
//! maps ready for downstream consumption.
//!
//! # Boot Sequence Position
//! Config -> **Instrument Download -> Universe Build** -> Auth -> WebSocket

// PR #6a (2026-05-19): bhavcopy_cross_check + bhavcopy_fetcher + bhavcopy_scheduler
// DELETED — 16:30 IST NSE bhavcopy cross-check retired under 4-IDX_I LOCKED_UNIVERSE.
pub mod binary_cache;
// PR #6a (2026-05-19): daily_scheduler + delta_detector RETIRED
// (4-IDX_I LOCKED_UNIVERSE — no daily Dhan CSV refresh = no day-over-day delta).
// PR #4 (2026-05-19): 6 depth modules DELETED.
// PR #6a (2026-05-19): diagnostic module RETIRED (no CSV download/parse/validate cycle).
// PR #4 (2026-05-19): `dynamic_subscription_state` module DELETED
// (depth-only diff state machine; no consumers under 4-IDX_I scope).
// PR #6b (2026-05-19): instrument_loader module RETIRED (boot uses FnoUniverse::locked_4_idx_i).
// PR #6b (2026-05-19): universe_builder + csv_downloader + csv_parser + validation
// modules RETIRED — boot reads LOCKED_UNIVERSE constant; no CSV pipeline.
pub mod market_open_self_test;
pub mod preopen_price_buffer;
pub mod slo_score;
pub mod subscription_distribution;
pub mod subscription_planner;

pub use binary_cache::MappedUniverse;
// PR #6a (2026-05-19): pub use diagnostic::run_instrument_diagnostic RETIRED.
// PR #6b (2026-05-19): instrument_loader + universe_builder re-exports RETIRED —
// boot uses FnoUniverse::locked_4_idx_i() instead of the dynamic loader.
pub use subscription_planner::{build_subscription_plan, build_subscription_plan_from_archived};
