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
pub mod broker_order_events;
pub mod build_info;
// Dead-code batch 2 (2026-07-18): `candle_fold` DELETED — zero consumers
// (the app's `rest_candle_fold` is a name-twin with its own types, not a
// consumer of this module).
pub mod config;
pub mod constants;
pub mod disconnect_cause;
pub mod error;
pub mod error_code;
pub mod feed;
pub mod feed_blame;
pub mod feed_health;
// Dead-code batch 2 (2026-07-18): `feed_parity` DELETED — the CANCELLED
// §33.2 Groww live-vs-backtest 1m comparer (zero refs; NOT the index_futures
// FUTIDX-02 seam, which stands). `formulas` DELETED — only ref was this decl.
pub mod instrument_registry;
pub mod instrument_types;
pub mod live_bar_freshness;
// PR-C3 (2026-07-14): `locked_universe` DELETED with the Dhan subscription
// surface (operator retirement directive 2026-07-13, scope-lock amendment
// §B item 2 — no Dhan WS subscription remains to lock a universe for).
pub mod market_hours;
pub mod moneyness;
pub mod muhurat;
// Dead-code batch 2 (2026-07-18): `open_price_rest_fallback` +
// `open_price_source` DELETED as a pair — the retired `/marketfeed/quote`
// open-price fallback (already a REMOVE class in
// no-rest-except-live-feed-2026-06-27.md §3; zero refs outside the pair).
pub mod order_types;
// Dead-code batch 2 (2026-07-18): `phase` DELETED — zero refs.
pub mod portfolio_types;
pub mod price_precision;
pub mod sanitize;
pub mod segment;
pub mod source_scan;
pub mod tick_types;
pub mod trading_calendar;
pub mod types;
pub mod url_join;
pub mod ws_event_types;
