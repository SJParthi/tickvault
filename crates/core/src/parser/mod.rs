//! Binary protocol parser for Dhan WebSocket V2.
//!
//! Parses raw binary frames into structured types. Entry point: `dispatch_frame()`.
//!
//! # Packet Types
//! - Ticker (16 bytes) — LTP + timestamp
//! - Quote (50 bytes) — LTP + volume + OHLC
//! - Full (162 bytes) — Quote + OI + 5-level depth
//! - OI (12 bytes) — standalone open interest update
//! - Previous Close (16 bytes) — previous close + previous OI
//! - Market Status (8 bytes) — header only
//! - Disconnect (10 bytes) — disconnect reason code

pub mod disconnect;
pub mod dispatcher;
pub mod full_packet;
pub mod header;
pub mod market_status;
pub mod oi;
pub mod previous_close;
pub mod quote;
pub mod ticker;
pub mod types;

// Re-export the main entry point and types for convenient use.
pub use dispatcher::dispatch_frame;
pub use types::{PacketHeader, ParseError, ParsedFrame};
