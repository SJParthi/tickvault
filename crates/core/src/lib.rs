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
//! - `auth` — Authentication, TOTP generation, JWT token lifecycle (Block 02)
//! - `instrument` — Master instrument download, CSV parsing, F&O universe building (Block 01)
//!
//! # Key Modules (to be built)
//! - `websocket_client` — Dhan WebSocket V2 connection lifecycle
//! - `binary_parser` — Zero-copy tick data parsing via zerocopy
//! - `tick_pipeline` — SPSC ring buffer routing to downstream consumers
//!
//! # Boot Sequence Position
//! Config -> Instrument Download -> **Auth** -> WebSocket -> Parse -> Route

pub mod auth;
pub mod historical;
pub mod instrument;
pub mod network;
pub mod notification;
pub mod parser;
pub mod pipeline;
pub mod websocket;
