//! WebSocket connection management, binary frame parsing, and tick data pipeline.
//!
//! # Key Modules (to be built)
//! - `websocket_client` — Dhan WebSocket V2 connection lifecycle
//! - `binary_parser` — Zero-copy tick data parsing via zerocopy
//! - `tick_pipeline` — SPSC ring buffer routing to downstream consumers
//!
//! # Boot Sequence Position
//! Auth -> **WebSocket -> Parse -> Route** -> Indicators
