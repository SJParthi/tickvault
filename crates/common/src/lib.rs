// Compile-time lint enforcement — defense-in-depth with CLI clippy flags
#![cfg_attr(not(test), deny(clippy::unwrap_used))]
#![cfg_attr(not(test), deny(clippy::expect_used))]
#![deny(clippy::print_stdout)]
#![deny(clippy::print_stderr)]
#![deny(clippy::dbg_macro)]
// Phase 0.2: no dropped Result/JoinHandle/must-use values (silent error swallowing).
#![cfg_attr(not(test), deny(unused_must_use))]
#![cfg_attr(not(test), warn(clippy::let_underscore_must_use))]
#![allow(missing_docs)] // TODO: enforce after adding docs to all public items

/// Shared types, constants, configuration, and error definitions
/// for the `tickvault` workspace.
///
/// This crate is imported by every other crate in the workspace.
/// It contains no business logic — only definitions and data structures.
pub mod config;
pub mod constants;
pub mod error;
pub mod error_code;
pub mod instrument_registry;
pub mod instrument_types;
pub mod market_hours;
pub mod mover_types;
pub mod order_types;
pub mod sanitize;
pub mod segment;
pub mod tick_types;
pub mod trading_calendar;
pub mod types;
