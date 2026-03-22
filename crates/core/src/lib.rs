// Compile-time lint enforcement — defense-in-depth with CLI clippy flags
#![cfg_attr(not(test), deny(clippy::unwrap_used))]
#![cfg_attr(not(test), deny(clippy::expect_used))]
#![deny(clippy::print_stdout)]
#![deny(clippy::print_stderr)]
#![deny(clippy::dbg_macro)]
#![allow(missing_docs)] // TODO: enforce after adding docs to all public items

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
pub mod historical;
pub mod index_constituency;
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
