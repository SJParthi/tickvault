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
pub mod instrument;
