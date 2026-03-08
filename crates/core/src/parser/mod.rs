//! Binary protocol parser for Dhan WebSocket V2.
//!
//! Parses raw binary frames into structured types. Entry point: `dispatch_frame()`.
//!
//! # Packet Types
//! - Index Ticker (16 bytes, code 1) — LTP + timestamp (index instruments)
//! - Ticker (16 bytes, code 2) — LTP + timestamp
//! - Market Depth (112 bytes, code 3) — LTP + 5-level depth (no timestamp)
//! - Quote (50 bytes, code 4) — LTP + volume + OHLC
//! - OI (12 bytes, code 5) — standalone open interest update
//! - Previous Close (16 bytes, code 6) — previous close + previous OI
//! - Market Status (8 bytes, code 7) — header only
//! - Full (162 bytes, code 8) — Quote + OI + 5-level depth
//! - Disconnect (10 bytes, code 50) — disconnect reason code

pub mod deep_depth;
pub mod disconnect;
pub mod dispatcher;
pub mod full_packet;
pub mod header;
pub mod market_depth;
pub mod market_status;
pub mod oi;
pub mod order_update;
pub mod previous_close;
pub mod quote;
mod read_helpers;
pub mod ticker;
pub mod types;

// Re-export the main entry point and types for convenient use.
pub use deep_depth::{
    DeepDepthHeader, DepthSide, ParsedDeepDepth, parse_twenty_depth_packet,
    parse_two_hundred_depth_packet,
};
pub use dispatcher::dispatch_frame;
pub use order_update::{OrderUpdateParseError, build_order_update_login, parse_order_update};
pub use types::{PacketHeader, ParseError, ParsedFrame};
