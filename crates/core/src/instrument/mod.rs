//! Master instrument download, parsing, and F&O universe building.
//!
//! Downloads Dhan's instrument master CSV daily, parses it, runs a 5-pass
//! mapping algorithm, and produces a complete `FnoUniverse` with all lookup
//! maps ready for downstream consumption.
//!
//! # Boot Sequence Position
//! Config -> **Instrument Download -> Universe Build** -> Auth -> WebSocket

pub mod binary_cache;
pub mod csv_downloader;
pub mod csv_parser;
pub mod daily_scheduler;
pub mod delta_detector;
pub mod diagnostic;
pub mod instrument_loader;
pub mod s3_backup;
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
