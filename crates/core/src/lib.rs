//! Core engine: instrument universe, WebSocket management, binary parsing, tick pipeline.
//!
//! # Modules
//! - `instrument` — Master instrument download, CSV parsing, F&O universe building
//!
//! # Key Modules (to be built)
//! - `websocket_client` — Dhan WebSocket V2 connection lifecycle
//! - `binary_parser` — Zero-copy tick data parsing via zerocopy
//! - `tick_pipeline` — SPSC ring buffer routing to downstream consumers
//!
//! # Boot Sequence Position
//! Config -> **Instrument Download -> Universe Build** -> Auth -> WebSocket -> Parse -> Route

pub mod instrument;
