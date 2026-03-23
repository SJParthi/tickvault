// Compile-time lint enforcement — defense-in-depth with CLI clippy flags
#![cfg_attr(not(test), deny(clippy::unwrap_used))]
#![cfg_attr(not(test), deny(clippy::expect_used))]
#![deny(clippy::print_stdout)]
#![deny(clippy::print_stderr)]
#![deny(clippy::dbg_macro)]
#![allow(missing_docs)] // TODO: enforce after adding docs to all public items

/// Shared types, constants, configuration, and error definitions
/// for the `dhan-live-trader` workspace.
///
/// This crate is imported by every other crate in the workspace.
/// It contains no business logic — only definitions and data structures.
pub mod config;
pub mod constants;
pub mod error;
pub mod instrument_registry;
pub mod instrument_types;
pub mod order_types;
pub mod sanitize;
pub mod segment;
pub mod tick_types;
pub mod trading_calendar;
pub mod types;
