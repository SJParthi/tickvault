/// Shared types, constants, configuration, and error definitions
/// for the dhan-live-trader workspace.
///
/// This crate is imported by every other crate in the workspace.
/// It contains no business logic — only definitions and data structures.
pub mod config;
pub mod constants;
pub mod error;
pub mod instrument_registry;
pub mod instrument_types;
pub mod sanitize;
pub mod tick_types;
pub mod types;
