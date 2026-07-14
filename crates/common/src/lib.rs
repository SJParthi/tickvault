// Compile-time lint enforcement — defense-in-depth with CLI clippy flags
#![cfg_attr(not(test), deny(clippy::unwrap_used))]
#![cfg_attr(not(test), deny(clippy::expect_used))]
#![deny(clippy::print_stdout)]
#![deny(clippy::print_stderr)]
#![deny(clippy::dbg_macro)]
// Phase 0.2: no dropped Result/JoinHandle/must-use values (silent error swallowing).
#![cfg_attr(not(test), deny(unused_must_use))]
#![cfg_attr(not(test), warn(clippy::let_underscore_must_use))]
#![allow(missing_docs)]
// TODO: enforce after adding docs to all public items
// APPROVED: clippy 1.95 tightened these doc-formatting lints; the codebase
// predates them. Allow rather than churn ~100 doc comments for a cosmetic
// markdown-rendering nicety with zero runtime/behavior impact.
#![allow(clippy::doc_lazy_continuation)]
#![allow(clippy::doc_overindented_list_items)]

/// Shared types, constants, configuration, and error definitions
/// for the `tickvault` workspace.
///
/// This crate is imported by every other crate in the workspace.
/// It contains no business logic — only definitions and data structures.
pub mod always_on;
pub mod build_info;
pub mod candle_fold;
pub mod config;
pub mod constants;
pub mod disconnect_cause;
pub mod error;
pub mod error_code;
pub mod feed;
pub mod feed_blame;
pub mod feed_health;
pub mod feed_parity;
pub mod formulas;
pub mod instrument_registry;
pub mod instrument_types;
pub mod live_bar_freshness;
pub mod locked_universe;
pub mod market_hours;
pub mod muhurat;
pub mod open_price_rest_fallback;
pub mod open_price_source;
pub mod order_types;
pub mod phase;
pub mod price_precision;
pub mod sanitize;
pub mod segment;
pub mod source_scan;
pub mod tick_types;
pub mod trading_calendar;
pub mod types;
pub mod url_join;
pub mod ws_event_types;
