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
pub mod csv_downloader;
pub mod csv_parser;
// PR #6a (2026-05-19): daily_scheduler + delta_detector RETIRED
// (4-IDX_I LOCKED_UNIVERSE — no daily Dhan CSV refresh = no day-over-day delta).
// PR #4 (2026-05-19): 6 depth modules DELETED.
pub mod diagnostic;
// PR #4 (2026-05-19): `dynamic_subscription_state` module DELETED
// (depth-only diff state machine; no consumers under 4-IDX_I scope).
pub mod instrument_loader;
pub mod market_open_self_test;
pub mod preopen_price_buffer;
pub mod slo_score;
pub mod subscription_distribution;
pub mod subscription_planner;
pub mod universe_builder;
pub mod validation;

pub use binary_cache::MappedUniverse;
pub use diagnostic::run_instrument_diagnostic;
pub use instrument_loader::{
    InstrumentLoadResult, load_or_build_instruments, try_rebuild_instruments,
};
pub use subscription_planner::{build_subscription_plan, build_subscription_plan_from_archived};
pub use universe_builder::{build_fno_universe, build_fno_universe_from_csv};
