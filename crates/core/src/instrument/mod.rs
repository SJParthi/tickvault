//! Master instrument download, parsing, and F&O universe building.
//!
//! Downloads Dhan's instrument master CSV daily, parses it, runs a 5-pass
//! mapping algorithm, and produces a complete `FnoUniverse` with all lookup
//! maps ready for downstream consumption.
//!
//! # Boot Sequence Position
//! Config -> **Instrument Download -> Universe Build** -> Auth -> WebSocket

pub mod bhavcopy_cross_check;
pub mod bhavcopy_fetcher;
pub mod bhavcopy_scheduler;
pub mod binary_cache;
pub mod csv_downloader;
pub mod csv_parser;
pub mod daily_scheduler;
pub mod delta_detector;
pub mod depth_200_dynamic_subscriber;
pub mod depth_20_dynamic_subscriber;
pub mod depth_dynamic_top_volume_selector;
pub mod depth_rebalancer;
pub mod depth_strike_selector;
pub mod depth_top_volume_selector;
pub mod diagnostic;
pub mod dynamic_subscription_state;
pub mod instrument_loader;
pub mod market_open_self_test;
pub mod phase2_readiness_check;
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
