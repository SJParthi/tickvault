// Compile-time lint enforcement — defense-in-depth with CLI clippy flags
#![cfg_attr(not(test), deny(clippy::unwrap_used))]
#![cfg_attr(not(test), deny(clippy::expect_used))]
#![deny(clippy::print_stdout)]
#![deny(clippy::print_stderr)]
#![deny(clippy::dbg_macro)]
// Phase 0.2: no dropped Result/JoinHandle/must-use values (silent error swallowing).
#![cfg_attr(not(test), deny(unused_must_use))]
#![cfg_attr(not(test), warn(clippy::let_underscore_must_use))]
#![cfg_attr(test, allow(clippy::assertions_on_constants))]
#![cfg_attr(test, allow(clippy::field_reassign_with_default))]
#![allow(missing_docs)]
// TODO: enforce after adding docs to all public items
// APPROVED: clippy 1.95 tightened these doc-formatting lints; the codebase
// predates them. Allow rather than churn ~100 doc comments for a cosmetic
// markdown-rendering nicety with zero runtime/behavior impact.
#![allow(clippy::doc_lazy_continuation)]
#![allow(clippy::doc_overindented_list_items)]

//! Core engine: instrument universe, authentication, WebSocket management,
//! binary parsing, tick pipeline.
//!
//! # Modules
//! - `auth` — Authentication, TOTP generation, JWT token lifecycle
//! - `historical` — Historical OHLCV candle fetching and chunked retrieval
//! - `index_constituency` — Index constituent mapping and validation
//! - `instrument` — Master instrument download, CSV parsing, F&O universe building
//! - `network` — IP monitoring, verification, and network health checks
//! - `notification` — Telegram alerting for critical events
//! - `parser` — Zero-copy binary packet parsing (ticker, quote, full, depth)
//! - `pipeline` — Tick processing and candle aggregation pipeline
//! - `websocket` — Dhan WebSocket V2 connection lifecycle, subscription, and reconnection
//!
//! # Boot Sequence Position
//! Config -> Instrument Download -> **Auth** -> WebSocket -> Parse -> Route

pub mod auth;
// PR-D (2026-05-26): `historical` module deleted — candle_fetcher.rs was
// the last surviving file (cross_verify chain deleted in PR-C). The
// entire Dhan historical fetch chain is gone.
// PR #6a (2026-05-19): index_constituency module RETIRED (4-IDX_I LOCKED_UNIVERSE).
pub mod instance_lock;
pub mod instrument;
pub mod network;
pub mod notification;
pub mod option_chain;
pub mod parser;
pub mod pipeline;
pub mod scheduler;
pub mod websocket;

#[cfg(test)]
pub mod test_support;
