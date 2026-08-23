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
//!
//! # Depth feeds (SEPARATE endpoints, different wire shape)
//! The depth-20 / depth-200 feeds use a **12-byte** header with `f64`
//! prices and 16-byte one-sided levels, and bid/ask arrive as SEPARATE
//! packets (codes 41 / 51). They are parsed by `depth.rs` via
//! `split_depth_frame()` + `dispatch_depth_packet()` — NEVER by
//! `dispatch_frame()`, whose 8-byte header would mis-read every field.

// ---------------------------------------------------------------------------
// Unguarded arithmetic is DENIED for this module and every child of it.
//
// Why here and not workspace-wide: the release profile pairs
// `overflow-checks = true` with `panic = "abort"`, so an integer overflow
// does not wrap and does not unwind — it KILLS the process. On this module,
// which decodes every packet off the wire, the input is attacker-shaped by
// definition: whatever the vendor sends is what we parse. An overflow here
// is the shortest path from a malformed frame to a dead trading process.
//
// A 2026-08-23 audit found this check switched off everywhere. The codebase
// was nonetheless disciplined about it — 764 guarded calls, and this whole
// module needed exactly ONE line changed to satisfy the lint — but that was
// convention, not enforcement, and a new `a + b` here would have compiled
// clean and passed CI. It no longer will.
//
// Scope is deliberate. The rest of the workspace has several hundred sites
// and turning it on globally is a separate, deliberate piece of work; this
// locks the path where the consequence is worst and the cost was one line.
// ---------------------------------------------------------------------------
#![deny(clippy::arithmetic_side_effects)]

// PR #4 (2026-05-19): `deep_depth` + `market_depth` parser modules DELETED.
// 2026-08-09: `depth` re-added for the depth-20 / depth-200 revival — a
// clean-room rebuild against `docs/dhan-ref/04-full-market-depth-websocket.md`,
// not a restore of the deleted modules.
pub mod depth;
pub mod disconnect;
pub mod dispatcher;
pub mod full_packet;
pub mod header;
pub mod market_status;
pub mod oi;
pub mod order_update;
pub mod previous_close;
pub mod quote;
mod read_helpers;
pub mod ticker;
pub mod truedata;
pub mod truedata_aux;
pub mod truedata_json;
pub mod truedata_requests;
pub mod truedata_router;
pub mod truedata_session;
pub mod types;

pub use depth::{
    DepthFeedKind, DepthFrameIter, DepthLevelBuffer, DepthPacket, DepthPacketHeader, DepthPayload,
    DepthSide, DepthSplitStop, depth_level_count, depth_packet_len, parse_depth_header,
    parse_depth_packet, split_depth_frame,
};
pub use dispatcher::{dispatch_depth_packet, dispatch_frame, prewarm_dispatcher_counters};
pub use order_update::{OrderUpdateParseError, build_order_update_login, parse_order_update};
pub use types::{PacketHeader, ParseError, ParsedFrame};
