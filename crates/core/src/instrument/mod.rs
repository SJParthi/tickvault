//! Master instrument download, parsing, and F&O universe building.
//!
//! Downloads Dhan's instrument master CSV daily, parses it, runs a 5-pass
//! mapping algorithm, and produces a complete `FnoUniverse` with all lookup
//! maps ready for downstream consumption.
//!
//! # Boot Sequence Position
//! Config -> **Instrument Download -> Universe Build** -> Auth -> WebSocket

pub mod csv_downloader;
pub mod csv_parser;
pub mod instrument_loader;
pub mod subscription_planner;
pub mod universe_builder;
pub mod validation;

pub use instrument_loader::{load_or_build_instruments, try_rebuild_instruments};
pub use subscription_planner::build_subscription_plan;
pub use universe_builder::{build_fno_universe, build_fno_universe_from_csv};
