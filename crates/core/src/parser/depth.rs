//! Dhan full-market-depth binary parsers — depth-20 and depth-200.
//!
//! # SCOPE: INDIA FEED ONLY
//! This module parses the INDIA depth feeds
//! (`depth-api-feed.dhan.co` / `full-depth-api.dhan.co`) and nothing else.
//! Dhan also ship a **US global-stocks feed** on a DIFFERENT host
//! (`wss://global-stocks-api-feed.dhan.co`, `25-global-stocks.md:544`) whose
//! packet tables look superficially similar but are a completely different
//! wire format: its `MsgCode` sits at **byte 10**, not byte 0
//! (`25-global-stocks.md:621`), and its packets are 37 / 27 / 19 bytes
//! (`:639`, `:653`, `:664`). Mixing the two would silently corrupt every
//! field. **Never source a layout, a code, or an offset from that file.**
//! Our codes are: 2 ticker, 4 quote, 5 OI, 6 prev-close, 8 full,
//! 50 disconnect, 41 depth bid, 51 depth ask.
//!
//! Ground truth: the operator-supplied Dhan API v2 pack,
//! `16-full-market-depth.md` (cited below as `16-fmd:<line>`), cross-checked
//! against the committed `docs/dhan-ref/04-full-market-depth-websocket.md`.
//! These packets arrive on the SEPARATE depth WebSocket endpoints
//! (`depth-api-feed.dhan.co` / `full-depth-api.dhan.co`), NOT on the main
//! feed, and they use a DIFFERENT wire shape from every packet in
//! `header.rs` / `ticker.rs` / `quote.rs` / `full_packet.rs`:
//!
//! | Aspect | Main feed | Depth feed |
//! |---|---|---|
//! | Header size | 8 bytes | **12 bytes** |
//! | Byte 0 | response code | **message length (u16 LE)** |
//! | Prices | `f32` | **`f64`** |
//! | Level size | 20 bytes (both sides) | **16 bytes (one side)** |
//! | Sides | combined in one packet | **separate packets (41 / 51)** |
//!
//! # Byte-numbering hazard (READ THIS BEFORE CHANGING AN OFFSET)
//! Dhan's header table is **1-BASED** while the packet tables describe the
//! same header as "0-12" and size it 12 bytes — thirteen numbers, twelve
//! bytes. A live example is `16-fmd:117`, verbatim:
//! `| 9-12 | uint32 | 4 | Message Sequence (to be ignored) |` — that field is
//! at 0-based `8..12`, which is what
//! `DEEP_DEPTH_HEADER_OFFSET_MSG_SEQUENCE == 8` encodes. A naive 0-based
//! reading shifts EVERY field by one byte and yields plausible-but-wrong
//! prices. The offsets below are the 0-based
//! truth, pinned by `crates/common/src/constants.rs` const-asserts and by
//! the golden-vector tests at the bottom of this file. Golden vectors are
//! the ONLY defence against this class of bug — never change an offset
//! without changing (and re-deriving) a golden vector.
//!
//! # Bid / ask code assignment — the doc contradicts itself
//! `41` = **Bid (BUY side)**, `51` = **Ask (SELL side)**.
//!
//! The vendor doc states both readings. Verified verbatim, 2026-08-09:
//! - Prose, `16-fmd:120` (and again at `:152` for the 200-level):
//!   *"you will receive the bid (sell) and ask (buy) data packets
//!   separately"* — i.e. bid=SELL, ask=BUY.
//! - Code table, `16-fmd:125-126` (and `:157-158`):
//!   *"`41` for Bid Data (Buy)"* / *"`51` for Ask Data (Sell)"* — i.e.
//!   bid=BUY, ask=SELL.
//!
//! **The CODE TABLE is authoritative** and is what this module implements: it
//! is the row that actually binds a numeric code to a side, it matches
//! universal market convention (bid = buy side), and it matches the committed
//! `docs/dhan-ref/04-full-market-depth-websocket.md` §5.1 + §10.4. The prose
//! parenthetical is a vendor typo. Getting this backwards would invert every
//! order book — hence the explicit tests below.
//!
//! # 20-level vs 200-level cannot be told apart from the bytes
//! Both feeds use codes 41/51 and the same 12-byte header. Header bytes
//! `8..12` mean **message sequence (ignore)** on depth-20 and **row count**
//! on depth-200 — the same four bytes, two meanings. Only the CONNECTION
//! knows which feed it is, so every entry point here takes an explicit
//! [`DepthFeedKind`]. Guessing is not possible and is not attempted.
//!
//! # Packet stacking
//! `16-fmd:169`, verbatim: *"Whenever 20 or 200 level depth packets are sent
//! on the connection, they are stacked one after another in a single message.
//! For 20 level depth, if two instruments are subscribed, then the first
//! instrument's Bid packet followed by Ask packet of that instrument is added
//! and then the second instrument's bid and ask packets in same sequence. To
//! handle this, you can break down the packet on the basis of length."*
//!
//! So a frame is `[I1 Bid][I1 Ask][I2 Bid][I2 Ask]…`. Callers MUST iterate
//! with [`split_depth_frame`] and never assume one packet per frame — that
//! exact layout is pinned by
//! `test_split_depth_frame_stacked_two_instruments_bid_then_ask_per_doc_169`.
//!
//! # Totality
//! Every function here is TOTAL over arbitrary bytes: truncated, oversized,
//! zero-length and garbage input all return a typed [`ParseError`] (or
//! terminate the iterator with a recorded [`DepthSplitStop`]). Nothing in
//! this module panics, indexes a slice unchecked, or allocates.
//!
//! # Performance
//! O(1) per level, zero heap allocation. The only loop is the bounded walk
//! over at most `TWO_HUNDRED_DEPTH_LEVELS` (200) levels, writing into a
//! caller-owned [`DepthLevelBuffer`] that the connection task allocates once
//! and reuses for the life of the socket.

use tickvault_common::constants::{
    DEEP_DEPTH_FEED_CODE_ASK, DEEP_DEPTH_FEED_CODE_BID, DEEP_DEPTH_HEADER_OFFSET_EXCHANGE_SEGMENT,
    DEEP_DEPTH_HEADER_OFFSET_FEED_CODE, DEEP_DEPTH_HEADER_OFFSET_MSG_LENGTH,
    DEEP_DEPTH_HEADER_OFFSET_MSG_SEQUENCE, DEEP_DEPTH_HEADER_OFFSET_SECURITY_ID,
    DEEP_DEPTH_HEADER_SIZE, DEEP_DEPTH_LEVEL_OFFSET_ORDERS, DEEP_DEPTH_LEVEL_OFFSET_PRICE,
    DEEP_DEPTH_LEVEL_OFFSET_QUANTITY, DEEP_DEPTH_LEVEL_SIZE, RESPONSE_CODE_DISCONNECT,
    TWENTY_DEPTH_LEVELS, TWO_HUNDRED_DEPTH_LEVELS,
};
use tickvault_common::tick_types::DeepDepthLevel;

use super::read_helpers::{read_f64_le, read_u16_le, read_u32_le};
use super::types::ParseError;

// ---------------------------------------------------------------------------
// Depth-feed disconnect packet
// ---------------------------------------------------------------------------

/// Size of a depth-feed disconnect packet: 12-byte header + i16 reason code.
///
/// NOTE this differs from the MAIN-FEED disconnect (10 bytes = 8-byte header
/// + i16) handled by `disconnect.rs`. Derived, never hardcoded.
pub const DEPTH_DISCONNECT_PACKET_SIZE: usize = DEEP_DEPTH_HEADER_SIZE + 2;

/// 0-based offset of the disconnect reason code within a depth disconnect
/// packet (immediately after the 12-byte header).
const DEPTH_DISCONNECT_OFFSET_REASON: usize = DEEP_DEPTH_HEADER_SIZE;

// ---------------------------------------------------------------------------
// Feed kind
// ---------------------------------------------------------------------------

/// Which depth feed a packet arrived on.
///
/// This is NOT derivable from the packet bytes (see the module docs) — the
/// caller supplies it from the connection it owns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DepthFeedKind {
    /// 20-level feed. Fixed 20 levels per packet; header bytes 8..12 are a
    /// message sequence that Dhan documents as "to be ignored".
    Twenty,
    /// 200-level feed. Variable level count; header bytes 8..12 are the ROW
    /// COUNT and the packet length is derived from it.
    TwoHundred,
}

// ---------------------------------------------------------------------------
// Side
// ---------------------------------------------------------------------------

/// Which side of the book a depth packet carries.
///
/// Bid and ask arrive as SEPARATE packets — a consumer needs both to build a
/// book.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DepthSide {
    /// Buy side — feed response code 41.
    Bid,
    /// Sell side — feed response code 51.
    Ask,
}

impl DepthSide {
    /// Maps a feed response code to a book side.
    ///
    /// Returns `None` for any code that is not 41 or 51 — never panics, so an
    /// unknown/garbage code is a typed refusal rather than a crash.
    #[inline]
    #[must_use]
    pub fn from_feed_code(code: u8) -> Option<Self> {
        match code {
            DEEP_DEPTH_FEED_CODE_BID => Some(Self::Bid),
            DEEP_DEPTH_FEED_CODE_ASK => Some(Self::Ask),
            _ => None,
        }
    }

    /// The wire feed response code for this side.
    #[inline]
    #[must_use]
    pub fn as_feed_code(self) -> u8 {
        match self {
            Self::Bid => DEEP_DEPTH_FEED_CODE_BID,
            Self::Ask => DEEP_DEPTH_FEED_CODE_ASK,
        }
    }
}

// ---------------------------------------------------------------------------
// Header
// ---------------------------------------------------------------------------

/// Parsed 12-byte depth-feed header.
///
/// Layout (0-based, little-endian):
/// `msg_length(u16) @0 + feed_code(u8) @2 + exchange_segment(u8) @3
///  + security_id(u32) @4 + sequence_or_row_count(u32) @8`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DepthPacketHeader {
    /// Vendor-declared *"Message Length of the entire payload packet"*
    /// (`16-fmd:113`, `:145`). That wording is ambiguous about whether the
    /// 12-byte header is included, so it is used ONLY as a CROSS-CHECK: the
    /// authoritative packet length is derived from the feed kind and (for
    /// depth-200) the row count. See [`depth_packet_len`] and
    /// [`DepthFrameIter::length_field_mismatches`]. UNVERIFIED-LIVE.
    pub message_length: u16,
    /// Feed response code: 41 = Bid, 51 = Ask, 50 = disconnect.
    pub response_code: u8,
    /// Raw exchange-segment byte. Deliberately NOT validated here: Dhan
    /// leaves values in the 0..=8 range unassigned (the design pack names
    /// 3, 6 and 7; our `ExchangeSegment::from_byte` currently accepts 3 and 7
    /// as currency segments and rejects only 6). Storing the raw byte means
    /// an unassigned or future value can never panic this parser — the
    /// consumer decides what to do with it.
    pub exchange_segment_code: u8,
    /// Security identifier — 4-byte LE on the wire, widened losslessly to
    /// `u64` to match `PacketHeader::security_id`.
    pub security_id: u64,
    /// Header bytes 8..12. Same bytes, two meanings — see [`DepthFeedKind`]:
    /// - depth-20: *"Message Sequence (to be ignored)"* (`16-fmd:117`). It is
    ///   decoded and named here, then DISCARDED. It is NOT a gap-detection
    ///   source: the doc explicitly says to ignore it, and no ordering or
    ///   continuity guarantee exists anywhere in the pack.
    /// - depth-200: *"No of Rows - gives number of rows to be read for
    ///   response"* (`16-fmd:149`) — the authoritative level count.
    pub sequence_or_row_count: u32,
}

/// Parses the 12-byte depth-feed header.
///
/// # Errors
/// [`ParseError::InsufficientBytes`] if `raw` is shorter than 12 bytes.
///
/// # Performance
/// O(1) — four fixed-offset `from_le_bytes` reads, no allocation.
#[inline]
#[allow(clippy::indexing_slicing)] // APPROVED: fixed-offset read; the caller/guard above proves the length
#[allow(clippy::arithmetic_side_effects)] // APPROVED: constant offsets bounded by the DEEP_DEPTH_HEADER_SIZE check above
pub fn parse_depth_header(raw: &[u8]) -> Result<DepthPacketHeader, ParseError> {
    if raw.len() < DEEP_DEPTH_HEADER_SIZE {
        return Err(ParseError::InsufficientBytes {
            expected: DEEP_DEPTH_HEADER_SIZE,
            actual: raw.len(),
        });
    }

    Ok(DepthPacketHeader {
        message_length: read_u16_le(raw, DEEP_DEPTH_HEADER_OFFSET_MSG_LENGTH),
        response_code: raw[DEEP_DEPTH_HEADER_OFFSET_FEED_CODE],
        exchange_segment_code: raw[DEEP_DEPTH_HEADER_OFFSET_EXCHANGE_SEGMENT],
        security_id: u64::from(read_u32_le(raw, DEEP_DEPTH_HEADER_OFFSET_SECURITY_ID)),
        sequence_or_row_count: read_u32_le(raw, DEEP_DEPTH_HEADER_OFFSET_MSG_SEQUENCE),
    })
}

// ---------------------------------------------------------------------------
// Level count / packet length
// ---------------------------------------------------------------------------

/// How many depth levels this packet carries.
///
/// Depth-20 is a fixed 20. Depth-200 reads the row count from header bytes
/// 8..12 — the packet is `12 + row_count * 16` bytes, which is normally far
/// less than the 3,212-byte maximum. NEVER assume a fixed depth-200 size.
///
/// # Errors
/// [`ParseError::InvalidRowCount`] if a depth-200 row count exceeds 200.
#[inline]
pub fn depth_level_count(
    header: &DepthPacketHeader,
    kind: DepthFeedKind,
) -> Result<usize, ParseError> {
    match kind {
        DepthFeedKind::Twenty => Ok(TWENTY_DEPTH_LEVELS),
        DepthFeedKind::TwoHundred => {
            let too_many = ParseError::InvalidRowCount {
                actual: header.sequence_or_row_count,
                max: TWO_HUNDRED_DEPTH_LEVELS,
            };
            // `try_from` cannot fail on a 64-bit target; using it (instead of
            // `as`) keeps the function total on any target width.
            let rows = usize::try_from(header.sequence_or_row_count).map_err(|_| too_many)?;
            if rows > TWO_HUNDRED_DEPTH_LEVELS {
                return Err(ParseError::InvalidRowCount {
                    actual: header.sequence_or_row_count,
                    max: TWO_HUNDRED_DEPTH_LEVELS,
                });
            }
            Ok(rows)
        }
    }
}

/// Total on-wire byte length of this packet, derived from the header.
///
/// This — not the vendor `message_length` field — is what the frame splitter
/// advances by, because it is derivable from the protocol shape and therefore
/// cannot be thrown off by a vendor-side length-field convention change.
/// `message_length` is retained as a cross-check (see
/// [`DepthFrameIter::length_field_mismatches`]).
///
/// # Errors
/// - [`ParseError::UnknownResponseCode`] for any code other than 41, 51 or 50.
/// - [`ParseError::InvalidRowCount`] for a depth-200 row count above 200 or an
///   arithmetic overflow computing the body size.
#[inline]
pub fn depth_packet_len(
    header: &DepthPacketHeader,
    kind: DepthFeedKind,
) -> Result<usize, ParseError> {
    if header.response_code == RESPONSE_CODE_DISCONNECT {
        return Ok(DEPTH_DISCONNECT_PACKET_SIZE);
    }
    if DepthSide::from_feed_code(header.response_code).is_none() {
        return Err(ParseError::UnknownResponseCode(header.response_code));
    }
    let levels = depth_level_count(header, kind)?;
    levels
        .checked_mul(DEEP_DEPTH_LEVEL_SIZE)
        .and_then(|body| body.checked_add(DEEP_DEPTH_HEADER_SIZE))
        .ok_or(ParseError::InvalidRowCount {
            actual: header.sequence_or_row_count,
            max: TWO_HUNDRED_DEPTH_LEVELS,
        })
}

// ---------------------------------------------------------------------------
// Reusable decode buffer
// ---------------------------------------------------------------------------

/// Caller-owned, reusable decode buffer for depth levels.
///
/// Sized for the depth-200 worst case (200 × 16 B of decoded state). The
/// connection task creates ONE of these and passes `&mut` to every
/// [`parse_depth_packet`] call, so the decode path never allocates and never
/// copies 3 KB of levels out by value.
#[derive(Debug, Clone)]
pub struct DepthLevelBuffer {
    levels: [DeepDepthLevel; TWO_HUNDRED_DEPTH_LEVELS],
}

impl DepthLevelBuffer {
    /// Creates a zeroed buffer. Allocate once per connection, reuse forever.
    #[inline]
    #[must_use]
    pub fn new() -> Self {
        Self {
            levels: [DeepDepthLevel::default(); TWO_HUNDRED_DEPTH_LEVELS],
        }
    }
}

impl Default for DepthLevelBuffer {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Parsed packet
// ---------------------------------------------------------------------------

/// The payload of a parsed depth packet.
#[derive(Debug, PartialEq)]
pub enum DepthPayload<'b> {
    /// One side of the book. `levels` borrows the caller's
    /// [`DepthLevelBuffer`] and is exactly as long as the packet's active
    /// level count (20 for depth-20; the header row count for depth-200,
    /// which may legitimately be 0).
    Levels {
        /// Which side of the book these levels belong to.
        side: DepthSide,
        /// The active levels, best price first.
        levels: &'b [DeepDepthLevel],
    },
    /// Server-initiated disconnect. The raw reason code is returned as-is;
    /// mapping it to a `DisconnectCode` is the caller's job, which keeps this
    /// module free of any `websocket` dependency.
    Disconnect {
        /// Raw disconnect reason code (e.g. 805 = >5 connections).
        reason_code: u16,
    },
}

/// A fully parsed depth packet.
#[derive(Debug, PartialEq)]
pub struct DepthPacket<'b> {
    /// The 12-byte header.
    pub header: DepthPacketHeader,
    /// The decoded payload.
    pub payload: DepthPayload<'b>,
}

/// Reads one 16-byte depth level at `base`.
///
/// Caller MUST have validated that `base + 16 <= raw.len()`.
#[inline(always)]
#[allow(clippy::indexing_slicing)] // APPROVED: fixed-offset read; the caller/guard above proves the length
#[allow(clippy::arithmetic_side_effects)] // APPROVED: base + constant offsets bounded by the caller's length check
// Level layout, `16-fmd:131-135` / `:163-167` (1-based in the doc):
//   `1-8` float64 Price | `9-12` uint32 Quantity | `13-16` uint32 No. of Orders
// 0-based: price @0, quantity @8, orders @12 — 16 bytes total.
fn parse_depth_level(raw: &[u8], base: usize) -> DeepDepthLevel {
    DeepDepthLevel {
        price: read_f64_le(raw, base + DEEP_DEPTH_LEVEL_OFFSET_PRICE),
        quantity: read_u32_le(raw, base + DEEP_DEPTH_LEVEL_OFFSET_QUANTITY),
        orders: read_u32_le(raw, base + DEEP_DEPTH_LEVEL_OFFSET_ORDERS),
    }
}

/// Parses ONE depth packet (already split out of its frame) into `buf`.
///
/// Handles depth-20, depth-200 and the depth-feed disconnect packet. For a
/// frame that may contain several stacked packets, drive this with
/// [`split_depth_frame`].
///
/// # Errors
/// - [`ParseError::InsufficientBytes`] — shorter than the header, or shorter
///   than the length derived from the header.
/// - [`ParseError::UnknownResponseCode`] — code is not 41, 51 or 50.
/// - [`ParseError::InvalidRowCount`] — depth-200 row count above 200.
///
/// # Performance
/// O(levels), bounded at 200. Zero heap allocation — levels are written into
/// the caller's buffer.
#[inline]
#[allow(clippy::indexing_slicing)] // APPROVED: fixed-offset read; the caller/guard above proves the length
#[allow(clippy::arithmetic_side_effects)] // APPROVED: `i` is bounded by `count` <= 200 and the length check above
pub fn parse_depth_packet<'b>(
    raw: &[u8],
    kind: DepthFeedKind,
    buf: &'b mut DepthLevelBuffer,
) -> Result<DepthPacket<'b>, ParseError> {
    let header = parse_depth_header(raw)?;
    let needed = depth_packet_len(&header, kind)?;
    if raw.len() < needed {
        return Err(ParseError::InsufficientBytes {
            expected: needed,
            actual: raw.len(),
        });
    }

    if header.response_code == RESPONSE_CODE_DISCONNECT {
        return Ok(DepthPacket {
            header,
            payload: DepthPayload::Disconnect {
                reason_code: read_u16_le(raw, DEPTH_DISCONNECT_OFFSET_REASON),
            },
        });
    }

    // `depth_packet_len` already refused any code that is not 41/51/50, and
    // the 50 arm returned above — so this cannot be `None`. The `ok_or` keeps
    // the function total instead of relying on that reasoning.
    let side = DepthSide::from_feed_code(header.response_code)
        .ok_or(ParseError::UnknownResponseCode(header.response_code))?;

    let count = depth_level_count(&header, kind)?;
    for (i, slot) in buf.levels.iter_mut().take(count).enumerate() {
        *slot = parse_depth_level(raw, DEEP_DEPTH_HEADER_SIZE + i * DEEP_DEPTH_LEVEL_SIZE);
    }

    // `count <= TWO_HUNDRED_DEPTH_LEVELS == buf.levels.len()`, so the slice is
    // always `Some`; the fallback keeps the function panic-free regardless.
    let levels = buf.levels.get(..count).unwrap_or(&[]);

    Ok(DepthPacket {
        header,
        payload: DepthPayload::Levels { side, levels },
    })
}

// ---------------------------------------------------------------------------
// Frame splitting
// ---------------------------------------------------------------------------

/// Why a [`DepthFrameIter`] stopped yielding packets.
///
/// Recorded rather than silently swallowed: a non-`Complete` reason means the
/// frame was malformed and the caller should count/log it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DepthSplitStop {
    /// The frame was consumed exactly — no trailing bytes.
    Complete,
    /// Trailing bytes remain but are too few for a 12-byte header.
    TruncatedHeader {
        /// Bytes left over.
        remaining: usize,
    },
    /// A header parsed but the declared body does not fit in the frame.
    TruncatedBody {
        /// Bytes the header says this packet needs.
        expected: usize,
        /// Bytes actually left in the frame.
        actual: usize,
    },
    /// A response code we cannot size, so we cannot know where the next
    /// packet starts. Splitting MUST stop — guessing would mis-frame
    /// everything after it.
    UnknownResponseCode(u8),
    /// A depth-200 row count above 200.
    InvalidRowCount(u32),
}

/// Iterator over the packets stacked inside one depth WebSocket frame.
///
/// A single frame may carry `[Inst1 Bid][Inst1 Ask][Inst2 Bid]…`. Each item is
/// a borrowed sub-slice of the original frame — no copying, no allocation.
///
/// # Termination
/// Every step advances by at least [`DEEP_DEPTH_HEADER_SIZE`] bytes, and any
/// non-advancing or unparseable condition sets [`DepthFrameIter::stop_reason`]
/// and fuses the iterator. It can neither loop forever nor panic.
#[derive(Debug)]
pub struct DepthFrameIter<'a> {
    raw: &'a [u8],
    kind: DepthFeedKind,
    offset: usize,
    stop: Option<DepthSplitStop>,
    length_field_mismatches: u32,
}

impl<'a> DepthFrameIter<'a> {
    /// Why iteration stopped. `None` until the iterator has finished.
    ///
    /// Anything other than `Some(DepthSplitStop::Complete)` after exhaustion
    /// means the frame was malformed.
    #[inline]
    #[must_use]
    pub fn stop_reason(&self) -> Option<DepthSplitStop> {
        self.stop
    }

    /// How many packets so far had a vendor `message_length` that disagreed
    /// with the length derived from the protocol shape.
    ///
    /// The derived length is authoritative (see [`depth_packet_len`]); this
    /// counter exists so a vendor-side convention change is LOUD instead of
    /// silently mis-framing the feed. Expected to be 0 in normal operation —
    /// this is UNVERIFIED-LIVE until the first depth session.
    #[inline]
    #[must_use]
    pub fn length_field_mismatches(&self) -> u32 {
        self.length_field_mismatches
    }

    /// Bytes of the frame consumed as complete packets so far.
    #[inline]
    #[must_use]
    pub fn consumed_bytes(&self) -> usize {
        self.offset
    }
}

impl<'a> Iterator for DepthFrameIter<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<Self::Item> {
        if self.stop.is_some() {
            return None;
        }

        let rest = match self.raw.get(self.offset..) {
            Some(rest) => rest,
            None => {
                self.stop = Some(DepthSplitStop::Complete);
                return None;
            }
        };
        if rest.is_empty() {
            self.stop = Some(DepthSplitStop::Complete);
            return None;
        }

        let header = match parse_depth_header(rest) {
            Ok(header) => header,
            Err(_) => {
                self.stop = Some(DepthSplitStop::TruncatedHeader {
                    remaining: rest.len(),
                });
                return None;
            }
        };

        let needed = match depth_packet_len(&header, self.kind) {
            Ok(needed) => needed,
            Err(ParseError::UnknownResponseCode(code)) => {
                self.stop = Some(DepthSplitStop::UnknownResponseCode(code));
                return None;
            }
            Err(_) => {
                self.stop = Some(DepthSplitStop::InvalidRowCount(
                    header.sequence_or_row_count,
                ));
                return None;
            }
        };

        // Belt-and-braces: a zero-length packet would make the iterator spin
        // forever. `depth_packet_len` can never return 0 (its floor is the
        // 12-byte header), but the loop-safety property is asserted here
        // rather than reasoned about at a distance.
        if needed == 0 {
            self.stop = Some(DepthSplitStop::InvalidRowCount(
                header.sequence_or_row_count,
            ));
            return None;
        }

        if usize::from(header.message_length) != needed {
            self.length_field_mismatches = self.length_field_mismatches.saturating_add(1);
        }

        let packet = match rest.get(..needed) {
            Some(packet) => packet,
            None => {
                self.stop = Some(DepthSplitStop::TruncatedBody {
                    expected: needed,
                    actual: rest.len(),
                });
                return None;
            }
        };

        self.offset = self.offset.saturating_add(needed);
        Some(packet)
    }
}

/// Splits a depth WebSocket frame into its stacked packets.
///
/// Advances by the length DERIVED from each header (fixed 332 bytes on
/// depth-20; `12 + row_count * 16` on depth-200; 14 for a disconnect) rather
/// than trusting the vendor `message_length` field, which is cross-checked and
/// counted instead. See [`DepthFrameIter`].
///
/// # Performance
/// O(packets), zero allocation — items borrow the input frame.
#[inline]
#[must_use]
pub fn split_depth_frame(raw: &[u8], kind: DepthFeedKind) -> DepthFrameIter<'_> {
    DepthFrameIter {
        raw,
        kind,
        offset: 0,
        stop: None,
        length_field_mismatches: 0,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------
//
// GOLDEN VECTORS ARE MANDATORY HERE. The depth header is documented 1-based
// while the packet tables are 0-based (module docs), so an off-by-one produces
// plausible-but-wrong prices rather than an obvious crash. The hand-derived
// byte arrays below are the only thing that catches that class of bug, and
// `test_parse_depth_header_byte_numbering_hazard_shift_changes_every_field`
// proves the vector is actually sensitive to a one-byte shift.

#[cfg(test)]
#[allow(clippy::indexing_slicing)] // APPROVED: fixed-offset read; the caller/guard above proves the length
#[allow(clippy::arithmetic_side_effects)] // APPROVED: test builders use constant offsets for packet construction
mod tests {
    use super::*;
    use proptest::prelude::*;

    // -----------------------------------------------------------------------
    // Hand-derived golden vector — depth-20 BID, first two levels
    // -----------------------------------------------------------------------
    //
    // Header (12 bytes, 0-based):
    //   @0..2  message_length = 332  = 0x014C -> 4C 01
    //   @2     feed code      = 41   = 0x29   (BID)
    //   @3     exch segment   = 1    = 0x01   (NSE_EQ)
    //   @4..8  security_id    = 1333 = 0x0535 -> 35 05 00 00
    //   @8..12 sequence       = 7             -> 07 00 00 00
    //
    // Level 0 @12..28:
    //   price    = 100.5  -> IEEE754 f64 0x4059200000000000 -> 00 00 00 00 00 20 59 40
    //   quantity = 250    = 0xFA -> FA 00 00 00
    //   orders   = 3             -> 03 00 00 00
    //
    // Level 1 @28..44:
    //   price    = 100.25 -> IEEE754 f64 0x4059100000000000 -> 00 00 00 00 00 10 59 40
    //   quantity = 100    = 0x64 -> 64 00 00 00
    //   orders   = 2             -> 02 00 00 00
    const GOLDEN_TWENTY_BID_PREFIX: [u8; 44] = [
        // header
        0x4C, 0x01, 0x29, 0x01, 0x35, 0x05, 0x00, 0x00, 0x07, 0x00, 0x00, 0x00,
        // level 0
        0x00, 0x00, 0x00, 0x00, 0x00, 0x20, 0x59, 0x40, 0xFA, 0x00, 0x00, 0x00, 0x03, 0x00, 0x00,
        0x00, // level 1
        0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x59, 0x40, 0x64, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00,
        0x00,
    ];

    /// The golden prefix, zero-padded to a full 332-byte depth-20 packet.
    fn golden_twenty_bid_packet() -> Vec<u8> {
        let mut buf =
            vec![0u8; DEEP_DEPTH_HEADER_SIZE + TWENTY_DEPTH_LEVELS * DEEP_DEPTH_LEVEL_SIZE];
        buf[..GOLDEN_TWENTY_BID_PREFIX.len()].copy_from_slice(&GOLDEN_TWENTY_BID_PREFIX);
        buf
    }

    // -----------------------------------------------------------------------
    // Builders (derived from the same offsets the golden vector pins)
    // -----------------------------------------------------------------------

    fn put_header(buf: &mut [u8], msg_len: u16, code: u8, segment: u8, sid: u32, seq_or_rows: u32) {
        buf[0..2].copy_from_slice(&msg_len.to_le_bytes());
        buf[2] = code;
        buf[3] = segment;
        buf[4..8].copy_from_slice(&sid.to_le_bytes());
        buf[8..12].copy_from_slice(&seq_or_rows.to_le_bytes());
    }

    fn put_level(buf: &mut [u8], index: usize, price: f64, quantity: u32, orders: u32) {
        let base = DEEP_DEPTH_HEADER_SIZE + index * DEEP_DEPTH_LEVEL_SIZE;
        buf[base..base + 8].copy_from_slice(&price.to_le_bytes());
        buf[base + 8..base + 12].copy_from_slice(&quantity.to_le_bytes());
        buf[base + 12..base + 16].copy_from_slice(&orders.to_le_bytes());
    }

    /// Builds a full depth-20 packet with `level i` = (100.0 + i, 10*(i+1), i+1).
    fn build_twenty(code: u8, segment: u8, sid: u32, seq: u32) -> Vec<u8> {
        let total = DEEP_DEPTH_HEADER_SIZE + TWENTY_DEPTH_LEVELS * DEEP_DEPTH_LEVEL_SIZE;
        let mut buf = vec![0u8; total];
        put_header(&mut buf, total as u16, code, segment, sid, seq);
        for i in 0..TWENTY_DEPTH_LEVELS {
            put_level(
                &mut buf,
                i,
                100.0 + i as f64,
                10 * (i as u32 + 1),
                i as u32 + 1,
            );
        }
        buf
    }

    /// Builds a depth-200 packet carrying exactly `rows` levels.
    fn build_two_hundred(code: u8, segment: u8, sid: u32, rows: u32) -> Vec<u8> {
        let count = rows as usize;
        let total = DEEP_DEPTH_HEADER_SIZE + count * DEEP_DEPTH_LEVEL_SIZE;
        let mut buf = vec![0u8; total];
        put_header(&mut buf, total as u16, code, segment, sid, rows);
        for i in 0..count {
            put_level(
                &mut buf,
                i,
                500.0 + i as f64 * 0.25,
                7 * (i as u32 + 1),
                i as u32 + 1,
            );
        }
        buf
    }

    /// Builds a depth-feed disconnect packet (12-byte header + i16 reason).
    fn build_depth_disconnect(reason: u16) -> Vec<u8> {
        let mut buf = vec![0u8; DEPTH_DISCONNECT_PACKET_SIZE];
        put_header(
            &mut buf,
            DEPTH_DISCONNECT_PACKET_SIZE as u16,
            RESPONSE_CODE_DISCONNECT,
            1,
            1333,
            0,
        );
        buf[DEEP_DEPTH_HEADER_SIZE..DEEP_DEPTH_HEADER_SIZE + 2]
            .copy_from_slice(&reason.to_le_bytes());
        buf
    }

    fn levels_of<'b>(packet: &'b DepthPacket<'b>) -> (DepthSide, &'b [DeepDepthLevel]) {
        match packet.payload {
            DepthPayload::Levels { side, levels } => (side, levels),
            DepthPayload::Disconnect { reason_code } => {
                panic!("expected Levels, got Disconnect({reason_code})")
            }
        }
    }

    // =======================================================================
    // parse_depth_header — golden vector + the byte-numbering hazard
    // =======================================================================

    #[test]
    fn test_parse_depth_header_golden_vector_twenty_bid() {
        let hdr = parse_depth_header(&GOLDEN_TWENTY_BID_PREFIX).unwrap();
        assert_eq!(hdr.message_length, 332, "msg_length @0..2");
        assert_eq!(hdr.response_code, DEEP_DEPTH_FEED_CODE_BID, "feed code @2");
        assert_eq!(hdr.exchange_segment_code, 1, "exchange segment @3");
        assert_eq!(hdr.security_id, 1333, "security_id @4..8");
        assert_eq!(hdr.sequence_or_row_count, 7, "sequence @8..12");
    }

    #[test]
    fn test_parse_depth_header_byte_numbering_hazard_shift_changes_every_field() {
        // A one-byte shift is EXACTLY the failure mode the 1-based/0-based
        // documentation mismatch produces. Feed the golden vector shifted by
        // one byte and prove every field decodes to something different — i.e.
        // the golden vector above is genuinely sensitive to the hazard.
        let good = parse_depth_header(&GOLDEN_TWENTY_BID_PREFIX).unwrap();
        let shifted = parse_depth_header(&GOLDEN_TWENTY_BID_PREFIX[1..]).unwrap();
        assert_ne!(good.message_length, shifted.message_length);
        assert_ne!(good.response_code, shifted.response_code);
        assert_ne!(good.security_id, shifted.security_id);
        // And a shifted read must NOT look like a legal side.
        assert!(DepthSide::from_feed_code(shifted.response_code).is_none());
    }

    #[test]
    fn test_parse_depth_header_is_twelve_bytes_not_eight() {
        // Guards against reusing the 8-byte MAIN-FEED header here.
        assert_eq!(DEEP_DEPTH_HEADER_SIZE, 12);
        let err = parse_depth_header(&[0u8; 11]).unwrap_err();
        let ParseError::InsufficientBytes { expected, actual } = err else {
            panic!("wrong variant")
        };
        assert_eq!(expected, 12);
        assert_eq!(actual, 11);
        assert!(parse_depth_header(&[0u8; 12]).is_ok());
    }

    #[test]
    fn test_parse_depth_header_short_inputs_never_panic() {
        for len in 0..DEEP_DEPTH_HEADER_SIZE {
            let buf = vec![0xAB_u8; len];
            assert!(parse_depth_header(&buf).is_err(), "len {len} must refuse");
        }
    }

    #[test]
    fn test_parse_depth_header_max_security_id_widens_losslessly() {
        let mut buf = [0u8; DEEP_DEPTH_HEADER_SIZE];
        put_header(
            &mut buf,
            332,
            DEEP_DEPTH_FEED_CODE_ASK,
            2,
            u32::MAX,
            u32::MAX,
        );
        let hdr = parse_depth_header(&buf).unwrap();
        assert_eq!(hdr.security_id, u64::from(u32::MAX));
        assert_eq!(hdr.sequence_or_row_count, u32::MAX);
    }

    // =======================================================================
    // DepthSide
    // =======================================================================

    #[test]
    fn test_depth_side_from_feed_code_41_is_bid_51_is_ask() {
        // `16-fmd:125-126` (code table, AUTHORITATIVE): 41 = Bid Data (Buy),
        // 51 = Ask Data (Sell). `16-fmd:120` (prose) says the opposite —
        // "bid (sell) and ask (buy)" — and is a vendor typo. Inverting this
        // would invert every order book, so it is pinned here explicitly.
        assert_eq!(DepthSide::from_feed_code(41), Some(DepthSide::Bid));
        assert_eq!(DepthSide::from_feed_code(51), Some(DepthSide::Ask));
    }

    #[test]
    fn test_depth_side_from_feed_code_rejects_every_other_byte() {
        for code in 0_u8..=255 {
            let expected = matches!(code, 41 | 51);
            assert_eq!(
                DepthSide::from_feed_code(code).is_some(),
                expected,
                "code {code}"
            );
        }
    }

    #[test]
    fn test_depth_side_as_feed_code_roundtrips() {
        for side in [DepthSide::Bid, DepthSide::Ask] {
            assert_eq!(DepthSide::from_feed_code(side.as_feed_code()), Some(side));
        }
        assert_eq!(DepthSide::Bid.as_feed_code(), 41);
        assert_eq!(DepthSide::Ask.as_feed_code(), 51);
    }

    // =======================================================================
    // depth_level_count / depth_packet_len
    // =======================================================================

    #[test]
    fn test_depth_level_count_twenty_is_always_twenty() {
        let mut buf = [0u8; DEEP_DEPTH_HEADER_SIZE];
        // Even with a wild "sequence" value, depth-20 is a fixed 20 levels —
        // bytes 8..12 are a sequence to be IGNORED on this feed.
        put_header(&mut buf, 332, DEEP_DEPTH_FEED_CODE_BID, 1, 1, u32::MAX);
        let hdr = parse_depth_header(&buf).unwrap();
        assert_eq!(depth_level_count(&hdr, DepthFeedKind::Twenty).unwrap(), 20);
    }

    #[test]
    fn test_depth_level_count_two_hundred_reads_row_count() {
        for rows in [0_u32, 1, 3, 199, 200] {
            let mut buf = [0u8; DEEP_DEPTH_HEADER_SIZE];
            put_header(&mut buf, 0, DEEP_DEPTH_FEED_CODE_BID, 2, 1, rows);
            let hdr = parse_depth_header(&buf).unwrap();
            assert_eq!(
                depth_level_count(&hdr, DepthFeedKind::TwoHundred).unwrap(),
                rows as usize
            );
        }
    }

    #[test]
    fn test_depth_level_count_two_hundred_refuses_oversized_row_count() {
        for rows in [201_u32, 1_000, 65_535, u32::MAX] {
            let mut buf = [0u8; DEEP_DEPTH_HEADER_SIZE];
            put_header(&mut buf, 0, DEEP_DEPTH_FEED_CODE_BID, 2, 1, rows);
            let hdr = parse_depth_header(&buf).unwrap();
            let err = depth_level_count(&hdr, DepthFeedKind::TwoHundred).unwrap_err();
            let ParseError::InvalidRowCount { actual, max } = err else {
                panic!("wrong variant for rows={rows}")
            };
            assert_eq!(actual, rows);
            assert_eq!(max, TWO_HUNDRED_DEPTH_LEVELS);
        }
    }

    #[test]
    fn test_depth_packet_len_twenty_is_332_and_two_hundred_is_derived() {
        let mut buf = [0u8; DEEP_DEPTH_HEADER_SIZE];
        put_header(&mut buf, 0, DEEP_DEPTH_FEED_CODE_BID, 1, 1, 5);
        let hdr = parse_depth_header(&buf).unwrap();
        assert_eq!(depth_packet_len(&hdr, DepthFeedKind::Twenty).unwrap(), 332);
        // 200-level length is NEVER fixed: 12 + rows * 16.
        assert_eq!(
            depth_packet_len(&hdr, DepthFeedKind::TwoHundred).unwrap(),
            12 + 5 * 16
        );
    }

    #[test]
    fn test_depth_packet_len_disconnect_is_fourteen() {
        let mut buf = [0u8; DEEP_DEPTH_HEADER_SIZE];
        put_header(&mut buf, 14, RESPONSE_CODE_DISCONNECT, 1, 1, 0);
        let hdr = parse_depth_header(&buf).unwrap();
        assert_eq!(DEPTH_DISCONNECT_PACKET_SIZE, 14);
        for kind in [DepthFeedKind::Twenty, DepthFeedKind::TwoHundred] {
            assert_eq!(depth_packet_len(&hdr, kind).unwrap(), 14);
        }
    }

    #[test]
    fn test_depth_packet_len_refuses_unknown_response_code() {
        for code in [0_u8, 1, 8, 40, 42, 50 + 1, 200, 255] {
            if matches!(code, 41 | 51 | 50) {
                continue;
            }
            let mut buf = [0u8; DEEP_DEPTH_HEADER_SIZE];
            put_header(&mut buf, 332, code, 1, 1, 0);
            let hdr = parse_depth_header(&buf).unwrap();
            let err = depth_packet_len(&hdr, DepthFeedKind::Twenty).unwrap_err();
            assert!(
                matches!(err, ParseError::UnknownResponseCode(c) if c == code),
                "code {code} -> {err:?}"
            );
        }
    }

    // =======================================================================
    // parse_depth_packet — golden vectors
    // =======================================================================

    #[test]
    fn test_parse_depth_packet_twenty_bid_golden_vector() {
        let raw = golden_twenty_bid_packet();
        let mut buf = DepthLevelBuffer::new();
        let packet = parse_depth_packet(&raw, DepthFeedKind::Twenty, &mut buf).unwrap();

        assert_eq!(packet.header.security_id, 1333);
        assert_eq!(packet.header.exchange_segment_code, 1);
        let (side, levels) = levels_of(&packet);
        assert_eq!(side, DepthSide::Bid);
        assert_eq!(levels.len(), 20);

        // Hand-derived expectations — see GOLDEN_TWENTY_BID_PREFIX above.
        assert!((levels[0].price - 100.5).abs() < f64::EPSILON);
        assert_eq!(levels[0].quantity, 250);
        assert_eq!(levels[0].orders, 3);
        assert!((levels[1].price - 100.25).abs() < f64::EPSILON);
        assert_eq!(levels[1].quantity, 100);
        assert_eq!(levels[1].orders, 2);
        // Remaining levels are the zero padding.
        assert_eq!(levels[2], DeepDepthLevel::default());
        assert_eq!(levels[19], DeepDepthLevel::default());
    }

    #[test]
    fn test_parse_depth_packet_twenty_ask_golden_vector() {
        // Same bytes, ask code — proves the side comes from byte 2 alone.
        let mut raw = golden_twenty_bid_packet();
        raw[DEEP_DEPTH_HEADER_OFFSET_FEED_CODE] = DEEP_DEPTH_FEED_CODE_ASK;
        let mut buf = DepthLevelBuffer::new();
        let packet = parse_depth_packet(&raw, DepthFeedKind::Twenty, &mut buf).unwrap();
        let (side, levels) = levels_of(&packet);
        assert_eq!(side, DepthSide::Ask);
        assert!((levels[0].price - 100.5).abs() < f64::EPSILON);
    }

    #[test]
    fn test_parse_depth_packet_twenty_all_levels_distinct() {
        let raw = build_twenty(DEEP_DEPTH_FEED_CODE_BID, 2, 49081, 42);
        let mut buf = DepthLevelBuffer::new();
        let packet = parse_depth_packet(&raw, DepthFeedKind::Twenty, &mut buf).unwrap();
        let (side, levels) = levels_of(&packet);
        assert_eq!(side, DepthSide::Bid);
        assert_eq!(levels.len(), TWENTY_DEPTH_LEVELS);
        for (i, level) in levels.iter().enumerate() {
            assert!(
                (level.price - (100.0 + i as f64)).abs() < f64::EPSILON,
                "level {i} price"
            );
            assert_eq!(level.quantity, 10 * (i as u32 + 1), "level {i} qty");
            assert_eq!(level.orders, i as u32 + 1, "level {i} orders");
        }
    }

    #[test]
    fn test_parse_depth_packet_two_hundred_bid_three_rows() {
        let raw = build_two_hundred(DEEP_DEPTH_FEED_CODE_BID, 2, 49081, 3);
        assert_eq!(raw.len(), 12 + 3 * 16, "200-depth length is DERIVED");
        let mut buf = DepthLevelBuffer::new();
        let packet = parse_depth_packet(&raw, DepthFeedKind::TwoHundred, &mut buf).unwrap();
        let (side, levels) = levels_of(&packet);
        assert_eq!(side, DepthSide::Bid);
        assert_eq!(levels.len(), 3);
        assert!((levels[0].price - 500.0).abs() < f64::EPSILON);
        assert!((levels[1].price - 500.25).abs() < f64::EPSILON);
        assert!((levels[2].price - 500.5).abs() < f64::EPSILON);
        assert_eq!(levels[2].quantity, 21);
        assert_eq!(levels[2].orders, 3);
    }

    #[test]
    fn test_parse_depth_packet_two_hundred_ask_full_200_rows() {
        let raw = build_two_hundred(
            DEEP_DEPTH_FEED_CODE_ASK,
            2,
            7,
            TWO_HUNDRED_DEPTH_LEVELS as u32,
        );
        assert_eq!(raw.len(), 12 + 200 * 16);
        let mut buf = DepthLevelBuffer::new();
        let packet = parse_depth_packet(&raw, DepthFeedKind::TwoHundred, &mut buf).unwrap();
        let (side, levels) = levels_of(&packet);
        assert_eq!(side, DepthSide::Ask);
        assert_eq!(levels.len(), 200);
        assert!((levels[199].price - (500.0 + 199.0 * 0.25)).abs() < f64::EPSILON);
        assert_eq!(levels[199].orders, 200);
    }

    #[test]
    fn test_parse_depth_packet_two_hundred_zero_rows_is_valid_and_empty() {
        // A legitimately empty book side: header only, zero levels.
        let raw = build_two_hundred(DEEP_DEPTH_FEED_CODE_BID, 2, 7, 0);
        assert_eq!(raw.len(), DEEP_DEPTH_HEADER_SIZE);
        let mut buf = DepthLevelBuffer::new();
        let packet = parse_depth_packet(&raw, DepthFeedKind::TwoHundred, &mut buf).unwrap();
        let (side, levels) = levels_of(&packet);
        assert_eq!(side, DepthSide::Bid);
        assert!(levels.is_empty());
    }

    #[test]
    fn test_parse_depth_packet_two_hundred_oversized_row_count_refused() {
        for rows in [201_u32, 500, u32::MAX] {
            let mut raw = vec![0u8; DEEP_DEPTH_HEADER_SIZE];
            put_header(&mut raw, 0, DEEP_DEPTH_FEED_CODE_BID, 2, 1, rows);
            let mut buf = DepthLevelBuffer::new();
            let err = parse_depth_packet(&raw, DepthFeedKind::TwoHundred, &mut buf).unwrap_err();
            assert!(
                matches!(err, ParseError::InvalidRowCount { actual, .. } if actual == rows),
                "rows {rows} -> {err:?}"
            );
        }
    }

    #[test]
    fn test_parse_depth_packet_truncated_body_is_refused_not_read() {
        let full = build_twenty(DEEP_DEPTH_FEED_CODE_BID, 1, 1333, 1);
        for cut in [12_usize, 100, 331] {
            let mut buf = DepthLevelBuffer::new();
            let err =
                parse_depth_packet(&full[..cut], DepthFeedKind::Twenty, &mut buf).unwrap_err();
            let ParseError::InsufficientBytes { expected, actual } = err else {
                panic!("cut {cut} -> wrong variant")
            };
            assert_eq!(expected, 332);
            assert_eq!(actual, cut);
        }
    }

    #[test]
    fn test_parse_depth_packet_two_hundred_truncated_body_is_refused() {
        let full = build_two_hundred(DEEP_DEPTH_FEED_CODE_ASK, 2, 7, 10);
        let mut buf = DepthLevelBuffer::new();
        let err = parse_depth_packet(&full[..full.len() - 1], DepthFeedKind::TwoHundred, &mut buf)
            .unwrap_err();
        assert!(
            matches!(err, ParseError::InsufficientBytes { .. }),
            "{err:?}"
        );
    }

    #[test]
    fn test_parse_depth_packet_unknown_response_code_is_typed_refusal() {
        let mut raw = build_twenty(DEEP_DEPTH_FEED_CODE_BID, 1, 1333, 1);
        raw[DEEP_DEPTH_HEADER_OFFSET_FEED_CODE] = 42;
        let mut buf = DepthLevelBuffer::new();
        let err = parse_depth_packet(&raw, DepthFeedKind::Twenty, &mut buf).unwrap_err();
        assert!(
            matches!(err, ParseError::UnknownResponseCode(42)),
            "{err:?}"
        );
    }

    #[test]
    fn test_parse_depth_packet_disconnect_returns_reason_code() {
        let raw = build_depth_disconnect(805);
        let mut buf = DepthLevelBuffer::new();
        let packet = parse_depth_packet(&raw, DepthFeedKind::Twenty, &mut buf).unwrap();
        assert_eq!(packet.header.response_code, RESPONSE_CODE_DISCONNECT);
        match packet.payload {
            DepthPayload::Disconnect { reason_code } => assert_eq!(reason_code, 805),
            DepthPayload::Levels { .. } => panic!("expected Disconnect"),
        }
    }

    #[test]
    fn test_parse_depth_packet_tolerates_unassigned_exchange_segments() {
        // Dhan leaves segment bytes unassigned (the design pack names 3, 6, 7).
        // The parser MUST store the raw byte and never panic for ANY value.
        for segment in 0_u8..=255 {
            let raw = build_twenty(DEEP_DEPTH_FEED_CODE_BID, segment, 1333, 1);
            let mut buf = DepthLevelBuffer::new();
            let packet = parse_depth_packet(&raw, DepthFeedKind::Twenty, &mut buf).unwrap();
            assert_eq!(packet.header.exchange_segment_code, segment);
        }
        // Explicit coverage of the three the design pack calls unassigned.
        for segment in [3_u8, 6, 7] {
            let raw = build_twenty(DEEP_DEPTH_FEED_CODE_ASK, segment, 1, 1);
            let mut buf = DepthLevelBuffer::new();
            assert!(parse_depth_packet(&raw, DepthFeedKind::Twenty, &mut buf).is_ok());
        }
    }

    #[test]
    fn test_parse_depth_packet_extra_trailing_bytes_are_ignored() {
        let mut raw = build_twenty(DEEP_DEPTH_FEED_CODE_BID, 1, 1333, 1);
        raw.extend_from_slice(&[0xFF; 64]);
        let mut buf = DepthLevelBuffer::new();
        let packet = parse_depth_packet(&raw, DepthFeedKind::Twenty, &mut buf).unwrap();
        let (_, levels) = levels_of(&packet);
        assert_eq!(levels.len(), 20);
    }

    #[test]
    fn test_depth_level_buffer_new_is_zeroed_and_reusable() {
        let mut buf = DepthLevelBuffer::new();
        let first = build_two_hundred(DEEP_DEPTH_FEED_CODE_BID, 2, 1, 5);
        {
            let packet = parse_depth_packet(&first, DepthFeedKind::TwoHundred, &mut buf).unwrap();
            assert_eq!(levels_of(&packet).1.len(), 5);
        }
        // Reuse for a SHORTER packet — the returned slice must be the new
        // length, never leaking stale levels from the previous decode.
        let second = build_two_hundred(DEEP_DEPTH_FEED_CODE_ASK, 2, 1, 2);
        let packet = parse_depth_packet(&second, DepthFeedKind::TwoHundred, &mut buf).unwrap();
        assert_eq!(levels_of(&packet).1.len(), 2);
        assert_eq!(
            DepthLevelBuffer::default().levels[0],
            DeepDepthLevel::default()
        );
    }

    // =======================================================================
    // split_depth_frame — stacked packets
    // =======================================================================

    #[test]
    fn test_split_depth_frame_stacked_bid_then_ask() {
        // A single WebSocket frame carrying [Inst1 Bid][Inst1 Ask].
        let mut frame = build_twenty(DEEP_DEPTH_FEED_CODE_BID, 1, 1333, 1);
        frame.extend_from_slice(&build_twenty(DEEP_DEPTH_FEED_CODE_ASK, 1, 1333, 2));

        let mut iter = split_depth_frame(&frame, DepthFeedKind::Twenty);
        let packets: Vec<&[u8]> = iter.by_ref().collect();
        assert_eq!(packets.len(), 2, "frame must split into 2 packets");
        assert_eq!(packets[0].len(), 332);
        assert_eq!(packets[1].len(), 332);
        assert_eq!(iter.stop_reason(), Some(DepthSplitStop::Complete));
        assert_eq!(iter.length_field_mismatches(), 0);
        assert_eq!(iter.consumed_bytes(), 664);

        let mut buf = DepthLevelBuffer::new();
        let bid = parse_depth_packet(packets[0], DepthFeedKind::Twenty, &mut buf).unwrap();
        assert_eq!(levels_of(&bid).0, DepthSide::Bid);
        let ask = parse_depth_packet(packets[1], DepthFeedKind::Twenty, &mut buf).unwrap();
        assert_eq!(levels_of(&ask).0, DepthSide::Ask);
    }

    /// The EXACT layout `16-fmd:169` specifies: per instrument, Bid then Ask,
    /// then the next instrument's bid and ask "in same sequence".
    #[test]
    fn test_split_depth_frame_stacked_two_instruments_bid_then_ask_per_doc_169() {
        let mut frame = Vec::new();
        for (sid, code) in [
            (1333_u32, DEEP_DEPTH_FEED_CODE_BID),
            (1333, DEEP_DEPTH_FEED_CODE_ASK),
            (2885, DEEP_DEPTH_FEED_CODE_BID),
            (2885, DEEP_DEPTH_FEED_CODE_ASK),
        ] {
            frame.extend_from_slice(&build_twenty(code, 1, sid, 0));
        }
        let mut iter = split_depth_frame(&frame, DepthFeedKind::Twenty);
        let decoded: Vec<(u64, Option<DepthSide>)> = iter
            .by_ref()
            .filter_map(|p| parse_depth_header(p).ok())
            .map(|h| (h.security_id, DepthSide::from_feed_code(h.response_code)))
            .collect();
        assert_eq!(
            decoded,
            vec![
                (1333, Some(DepthSide::Bid)),
                (1333, Some(DepthSide::Ask)),
                (2885, Some(DepthSide::Bid)),
                (2885, Some(DepthSide::Ask)),
            ],
            "16-fmd:169 order: per instrument Bid then Ask, then next instrument"
        );
        assert_eq!(iter.stop_reason(), Some(DepthSplitStop::Complete));
    }

    #[test]
    fn test_split_depth_frame_two_hundred_variable_lengths_stack() {
        // Depth-200 packets have DIFFERENT lengths — the splitter must derive
        // each one from its own row count, not assume a fixed size.
        let mut frame = build_two_hundred(DEEP_DEPTH_FEED_CODE_BID, 2, 7, 3);
        frame.extend_from_slice(&build_two_hundred(DEEP_DEPTH_FEED_CODE_ASK, 2, 7, 17));
        let mut iter = split_depth_frame(&frame, DepthFeedKind::TwoHundred);
        let lens: Vec<usize> = iter.by_ref().map(<[u8]>::len).collect();
        assert_eq!(lens, vec![12 + 3 * 16, 12 + 17 * 16]);
        assert_eq!(iter.stop_reason(), Some(DepthSplitStop::Complete));
    }

    #[test]
    fn test_split_depth_frame_empty_frame_yields_nothing() {
        let mut iter = split_depth_frame(&[], DepthFeedKind::Twenty);
        assert!(iter.next().is_none());
        assert_eq!(iter.stop_reason(), Some(DepthSplitStop::Complete));
        assert_eq!(iter.consumed_bytes(), 0);
    }

    #[test]
    fn test_split_depth_frame_truncated_tail_is_reported_not_yielded() {
        let mut frame = build_twenty(DEEP_DEPTH_FEED_CODE_BID, 1, 1333, 1);
        let partial = build_twenty(DEEP_DEPTH_FEED_CODE_ASK, 1, 1333, 2);
        frame.extend_from_slice(&partial[..100]); // second packet cut short
        let mut iter = split_depth_frame(&frame, DepthFeedKind::Twenty);
        let packets: Vec<&[u8]> = iter.by_ref().collect();
        assert_eq!(packets.len(), 1, "only the complete packet is yielded");
        assert_eq!(
            iter.stop_reason(),
            Some(DepthSplitStop::TruncatedBody {
                expected: 332,
                actual: 100
            })
        );
    }

    #[test]
    fn test_split_depth_frame_truncated_header_tail_is_reported() {
        let mut frame = build_twenty(DEEP_DEPTH_FEED_CODE_BID, 1, 1333, 1);
        frame.extend_from_slice(&[0xAA; 5]); // fewer than 12 header bytes
        let mut iter = split_depth_frame(&frame, DepthFeedKind::Twenty);
        assert_eq!(iter.by_ref().count(), 1);
        assert_eq!(
            iter.stop_reason(),
            Some(DepthSplitStop::TruncatedHeader { remaining: 5 })
        );
    }

    #[test]
    fn test_split_depth_frame_unknown_code_stops_rather_than_guessing() {
        let mut frame = build_twenty(DEEP_DEPTH_FEED_CODE_BID, 1, 1333, 1);
        let mut bogus = build_twenty(DEEP_DEPTH_FEED_CODE_ASK, 1, 1333, 2);
        bogus[DEEP_DEPTH_HEADER_OFFSET_FEED_CODE] = 99;
        frame.extend_from_slice(&bogus);
        let mut iter = split_depth_frame(&frame, DepthFeedKind::Twenty);
        assert_eq!(iter.by_ref().count(), 1);
        assert_eq!(
            iter.stop_reason(),
            Some(DepthSplitStop::UnknownResponseCode(99))
        );
    }

    #[test]
    fn test_split_depth_frame_invalid_row_count_stops() {
        let mut frame = vec![0u8; DEEP_DEPTH_HEADER_SIZE];
        put_header(&mut frame, 0, DEEP_DEPTH_FEED_CODE_BID, 2, 1, 9_999);
        let mut iter = split_depth_frame(&frame, DepthFeedKind::TwoHundred);
        assert_eq!(iter.by_ref().count(), 0);
        assert_eq!(
            iter.stop_reason(),
            Some(DepthSplitStop::InvalidRowCount(9_999))
        );
    }

    #[test]
    fn test_split_depth_frame_length_field_mismatches_are_counted() {
        // The DERIVED length is authoritative; a vendor length-field change
        // must be counted (loud) rather than silently mis-framing the feed.
        let mut frame = build_twenty(DEEP_DEPTH_FEED_CODE_BID, 1, 1333, 1);
        frame[0..2].copy_from_slice(&320_u16.to_le_bytes()); // body-only length
        let mut iter = split_depth_frame(&frame, DepthFeedKind::Twenty);
        assert_eq!(iter.by_ref().count(), 1, "still framed correctly");
        assert_eq!(iter.length_field_mismatches(), 1);
        assert_eq!(iter.stop_reason(), Some(DepthSplitStop::Complete));
    }

    #[test]
    fn test_split_depth_frame_stacked_disconnect_packet() {
        let mut frame = build_twenty(DEEP_DEPTH_FEED_CODE_BID, 1, 1333, 1);
        frame.extend_from_slice(&build_depth_disconnect(805));
        let mut iter = split_depth_frame(&frame, DepthFeedKind::Twenty);
        let packets: Vec<&[u8]> = iter.by_ref().collect();
        assert_eq!(packets.len(), 2);
        assert_eq!(packets[1].len(), DEPTH_DISCONNECT_PACKET_SIZE);
        assert_eq!(iter.stop_reason(), Some(DepthSplitStop::Complete));
    }

    #[test]
    fn test_split_depth_frame_stop_reason_is_none_until_exhausted() {
        // `stop_reason()` is the caller's only way to tell "this frame ended
        // cleanly" from "this frame was malformed and I silently got fewer
        // packets than the wire carried". It must stay None while iteration is
        // still in progress, or a caller polling mid-loop would read a clean
        // frame as an incomplete one.
        let mut frame = build_twenty(DEEP_DEPTH_FEED_CODE_BID, 1, 1333, 1);
        frame.extend_from_slice(&build_twenty(DEEP_DEPTH_FEED_CODE_ASK, 1, 1333, 2));

        let mut iter = split_depth_frame(&frame, DepthFeedKind::Twenty);
        assert_eq!(iter.stop_reason(), None, "not started");

        assert!(iter.next().is_some(), "first packet");
        assert_eq!(
            iter.stop_reason(),
            None,
            "one packet still pending — must not report a stop yet"
        );

        assert!(iter.next().is_some(), "second packet");
        assert!(iter.next().is_none(), "exhausted");
        assert_eq!(
            iter.stop_reason(),
            Some(DepthSplitStop::Complete),
            "both packets consumed exactly, so the frame is Complete"
        );
    }

    #[test]
    fn test_split_depth_frame_stop_reason_reports_truncated_header_on_trailing_bytes() {
        // Non-vacuity for the test above: a frame that does NOT end cleanly
        // must report a non-Complete reason, so `Complete` actually carries
        // information.
        let mut frame = build_twenty(DEEP_DEPTH_FEED_CODE_BID, 1, 1333, 1);
        frame.extend_from_slice(&[0xAA; 5]); // fewer than the 12-byte header

        let mut iter = split_depth_frame(&frame, DepthFeedKind::Twenty);
        assert_eq!(iter.by_ref().count(), 1, "the one whole packet is yielded");
        assert_eq!(
            iter.stop_reason(),
            Some(DepthSplitStop::TruncatedHeader { remaining: 5 }),
            "the 5 trailing bytes must be reported, never silently dropped"
        );
    }

    #[test]
    fn test_split_depth_frame_consumed_bytes_counts_whole_packets_only() {
        // `consumed_bytes()` must advance by exactly one packet per yield and
        // must NOT include a trailing partial packet — a caller uses the
        // difference against the frame length to size what was discarded.
        let one = build_twenty(DEEP_DEPTH_FEED_CODE_BID, 1, 1333, 1);
        let packet_len = one.len();

        let mut frame = one.clone();
        frame.extend_from_slice(&build_twenty(DEEP_DEPTH_FEED_CODE_ASK, 1, 1333, 2));
        frame.extend_from_slice(&[0xAA; 5]);

        let mut iter = split_depth_frame(&frame, DepthFeedKind::Twenty);
        assert_eq!(
            iter.consumed_bytes(),
            0,
            "nothing consumed before the first yield"
        );

        assert!(iter.next().is_some());
        assert_eq!(iter.consumed_bytes(), packet_len);

        assert!(iter.next().is_some());
        assert_eq!(iter.consumed_bytes(), packet_len * 2);

        assert!(iter.next().is_none());
        assert_eq!(
            iter.consumed_bytes(),
            packet_len * 2,
            "the 5 trailing bytes are NOT consumed — they were never a packet"
        );
        assert_eq!(
            frame.len() - iter.consumed_bytes(),
            5,
            "frame length minus consumed must equal the discarded remainder"
        );
    }

    #[test]
    fn test_split_depth_frame_terminates_on_deterministic_garbage() {
        // Deterministic LCG garbage of many lengths: the iterator must always
        // terminate, never panic, and never claim more bytes than it was given.
        let mut state: u32 = 0x1234_5678;
        for len in 0..600_usize {
            let mut frame = Vec::with_capacity(len);
            for _ in 0..len {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                frame.push((state >> 24) as u8);
            }
            for kind in [DepthFeedKind::Twenty, DepthFeedKind::TwoHundred] {
                let mut iter = split_depth_frame(&frame, kind);
                let mut steps = 0_usize;
                for packet in iter.by_ref() {
                    assert!(packet.len() >= DEEP_DEPTH_HEADER_SIZE);
                    steps += 1;
                    assert!(steps <= len + 1, "iterator failed to make progress");
                }
                assert!(iter.stop_reason().is_some());
                assert!(iter.consumed_bytes() <= len);
            }
        }
    }

    #[test]
    fn test_parse_depth_packet_never_panics_on_deterministic_garbage() {
        let mut state: u32 = 0xDEAD_BEEF;
        let mut buf = DepthLevelBuffer::new();
        for len in 0..400_usize {
            let mut frame = Vec::with_capacity(len);
            for _ in 0..len {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                frame.push((state >> 16) as u8);
            }
            for kind in [DepthFeedKind::Twenty, DepthFeedKind::TwoHundred] {
                // Result deliberately ignored — the assertion is "does not panic".
                let _ = parse_depth_packet(&frame, kind, &mut buf);
            }
        }
    }

    // =======================================================================
    // Property tests — totality over arbitrary bytes
    // =======================================================================

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(512))]

        /// The parser is TOTAL: arbitrary bytes never panic, and a success
        /// always yields a level count consistent with the feed kind.
        #[test]
        fn test_parse_depth_packet_is_total_over_arbitrary_bytes(
            bytes in proptest::collection::vec(any::<u8>(), 0..800),
            two_hundred in any::<bool>(),
        ) {
            let kind = if two_hundred { DepthFeedKind::TwoHundred } else { DepthFeedKind::Twenty };
            let mut buf = DepthLevelBuffer::new();
            if let Ok(packet) = parse_depth_packet(&bytes, kind, &mut buf) {
                match packet.payload {
                    DepthPayload::Levels { levels, .. } => {
                        prop_assert!(levels.len() <= TWO_HUNDRED_DEPTH_LEVELS);
                        if kind == DepthFeedKind::Twenty {
                            prop_assert_eq!(levels.len(), TWENTY_DEPTH_LEVELS);
                        }
                    }
                    DepthPayload::Disconnect { .. } => {
                        prop_assert!(bytes.len() >= DEPTH_DISCONNECT_PACKET_SIZE);
                    }
                }
            }
        }

        /// The splitter always terminates, always makes progress, and never
        /// consumes more than the frame it was given.
        #[test]
        fn test_split_depth_frame_always_terminates_and_progresses(
            bytes in proptest::collection::vec(any::<u8>(), 0..800),
            two_hundred in any::<bool>(),
        ) {
            let kind = if two_hundred { DepthFeedKind::TwoHundred } else { DepthFeedKind::Twenty };
            let mut iter = split_depth_frame(&bytes, kind);
            let mut total = 0_usize;
            let mut steps = 0_usize;
            for packet in iter.by_ref() {
                // Every step consumes at least a full header, so the iterator
                // provably makes progress and cannot spin.
                prop_assert!(packet.len() >= DEEP_DEPTH_HEADER_SIZE);
                total = total.saturating_add(packet.len());
                steps += 1;
                prop_assert!(steps <= bytes.len(), "unbounded iteration");
            }
            prop_assert!(iter.stop_reason().is_some());
            prop_assert_eq!(iter.consumed_bytes(), total, "consumed == sum of packet lengths");
            prop_assert!(iter.consumed_bytes() <= bytes.len());
        }

        /// Round-trip: any well-formed depth-200 packet decodes back to the
        /// exact levels that were encoded, at any legal row count.
        #[test]
        fn test_parse_depth_packet_two_hundred_roundtrip(rows in 0_u32..=200) {
            let raw = build_two_hundred(DEEP_DEPTH_FEED_CODE_BID, 2, 7, rows);
            let mut buf = DepthLevelBuffer::new();
            let packet = parse_depth_packet(&raw, DepthFeedKind::TwoHundred, &mut buf)
                .map_err(|e| TestCaseError::fail(format!("{e:?}")))?;
            match packet.payload {
                DepthPayload::Levels { levels, .. } => {
                    prop_assert_eq!(levels.len(), rows as usize);
                    for (i, level) in levels.iter().enumerate() {
                        prop_assert!((level.price - (500.0 + i as f64 * 0.25)).abs() < f64::EPSILON);
                        prop_assert_eq!(level.quantity, 7 * (i as u32 + 1));
                    }
                }
                DepthPayload::Disconnect { .. } => prop_assert!(false, "expected levels"),
            }
        }
    }

    // =======================================================================
    // ORDER-BOOK ORDERING INVARIANTS
    // =======================================================================
    //
    // WHY THIS SECTION EXISTS (2026-08-25). Every one of the 46 tests above
    // asserts a FIELD VALUE from a golden vector (`levels[0].orders == 3`).
    // Not one asserts a RELATION BETWEEN levels. That is the same blind spot
    // that let the ~9.2x candle-volume inflation ship green this morning:
    // every part was tested, and no test asked whether the parts AGREE.
    //
    // The concrete bug class this catches: a level-index or side-code drift
    // in the fixed-offset walk (`DEEP_DEPTH_HEADER_SIZE + i * LEVEL_SIZE`)
    // produces a book in which EVERY FIELD DECODES CORRECTLY and every
    // golden-value test still passes — the levels are simply in the wrong
    // order, or attributed to the wrong side. Depth is the largest payload
    // in the system, so a scrambled book is also the most expensive one.
    //
    // WHAT AN "ABSENT" LEVEL IS (read from the parser, not assumed).
    // `parse_depth_packet` has NO empty-level sentinel: it decodes whatever
    // bytes are there, and `DeepDepthLevel::default()` is all-zero. Depth-20
    // is a FIXED 20 levels on the wire regardless of how many the exchange
    // actually populated, so the tail is zero-padding — exactly what
    // `golden_twenty_bid_packet()` reproduces. An all-zero level is
    // therefore ABSENT, not "a resting order at price 0", and is excluded
    // from the price-ordering comparison EXPLICITLY below rather than
    // slipping through because `100.25 >= 0.0` happens to hold.
    //
    // WHERE THE UNCROSSED RULE DOES *NOT* HOLD (honest limit, do not delete).
    // NSE's pre-open call auction (09:00-09:08 IST) legitimately produces a
    // CROSSED book — buy orders above sell orders crossing is how the
    // auction discovers the equilibrium price. `trading_calendar.rs` sets
    // `data_collection_start = "09:00:00"`, so captured depth genuinely
    // CONTAINS that window. `check_book_uncrossed` therefore encodes the
    // CONTINUOUS-SESSION rule and is applied here to constructed packets
    // only. It must NEVER be promoted to a runtime reject/alarm without a
    // continuous-session gate, or every trading morning pages falsely.
    // No vendor doc states a crossed-book rule either way (searched
    // `docs/dhan-ref/04-full-market-depth-websocket.md`, zero hits) — this
    // is market structure, not vendor contract, and is labelled as such.

    /// True when a level carries no resting order and is therefore padding
    /// rather than a price point. See the section header: the parser has no
    /// sentinel, so absence is "no size and no orders", plus a defensive
    /// guard on a non-positive / non-finite price (never a real NSE price).
    fn depth_level_is_absent(level: &DeepDepthLevel) -> bool {
        (level.quantity == 0 && level.orders == 0) || !(level.price > 0.0)
    }

    /// Bid prices must be NON-INCREASING by level: level 0 is the best (i.e.
    /// highest) buy price and each further level is worse or equal. Equal is
    /// permitted — two price points can legitimately tie after a tick-size
    /// rounding, and refusing ties would make this fire on real data.
    fn check_bid_ordering(levels: &[DeepDepthLevel]) -> Result<(), String> {
        let live: Vec<&DeepDepthLevel> = levels
            .iter()
            .filter(|l| !depth_level_is_absent(l))
            .collect();
        for (i, pair) in live.windows(2).enumerate() {
            if pair[0].price < pair[1].price {
                return Err(format!(
                    "BID ORDERING VIOLATED: level {i} price {} < level {} price {} \
                     (bids must be non-increasing, best/highest first)",
                    pair[0].price,
                    i + 1,
                    pair[1].price
                ));
            }
        }
        Ok(())
    }

    /// Ask prices must be NON-DECREASING by level: level 0 is the best (i.e.
    /// lowest) sell price and each further level is worse or equal.
    fn check_ask_ordering(levels: &[DeepDepthLevel]) -> Result<(), String> {
        let live: Vec<&DeepDepthLevel> = levels
            .iter()
            .filter(|l| !depth_level_is_absent(l))
            .collect();
        for (i, pair) in live.windows(2).enumerate() {
            if pair[0].price > pair[1].price {
                return Err(format!(
                    "ASK ORDERING VIOLATED: level {i} price {} > level {} price {} \
                     (asks must be non-decreasing, best/lowest first)",
                    pair[0].price,
                    i + 1,
                    pair[1].price
                ));
            }
        }
        Ok(())
    }

    /// Populated levels must form a PREFIX — no hole between two live levels.
    /// A book is ranked best-first, so "level 3 empty, level 4 populated" is
    /// not a market state; it is precisely what a level-index drift looks
    /// like, and no field-value assertion can see it.
    fn check_populated_levels_are_a_prefix(levels: &[DeepDepthLevel]) -> Result<(), String> {
        let mut seen_absent_at: Option<usize> = None;
        for (i, level) in levels.iter().enumerate() {
            if depth_level_is_absent(level) {
                if seen_absent_at.is_none() {
                    seen_absent_at = Some(i);
                }
            } else if let Some(hole) = seen_absent_at {
                return Err(format!(
                    "DEPTH HOLE: level {hole} is absent but level {i} is populated \
                     (price {}, qty {}) — populated levels must be a contiguous prefix",
                    level.price, level.quantity
                ));
            }
        }
        Ok(())
    }

    /// Best bid must be strictly BELOW best ask wherever both sides have at
    /// least one populated level. Continuous-session rule only — see the
    /// pre-open carve-out in the section header.
    fn check_book_uncrossed(
        bids: &[DeepDepthLevel],
        asks: &[DeepDepthLevel],
    ) -> Result<(), String> {
        // O(1) EXEMPT: test-only checker inside `#[cfg(test)] mod tests` — it never
        // runs in production and never touches the hot path. Reading index 0 WOULD be
        // O(1) and is correct for a well-formed book (populated levels are a prefix),
        // but this checker must stay honest on a MALFORMED one: with level 0 empty and
        // level 1 populated, an index-0 read reports "side empty" and silently skips
        // the crossed-book test — a false OK on exactly the drift this exists to catch.
        // The scan is bounded by the level count (≤200) and runs once per test.
        let best_bid = bids.iter().find(|l| !depth_level_is_absent(l)); // O(1) EXEMPT: see above
        let best_ask = asks.iter().find(|l| !depth_level_is_absent(l)); // O(1) EXEMPT: see above
        match (best_bid, best_ask) {
            (Some(bid), Some(ask)) if bid.price >= ask.price => Err(format!(
                "BOOK CROSSED: best bid {} >= best ask {} \
                 (continuous session; pre-open auction is exempt)",
                bid.price, ask.price
            )),
            _ => Ok(()),
        }
    }

    /// Builds a REALISTIC depth-20 book side: bids descend from `best`, asks
    /// ascend from `best`, both in 0.05 tick steps.
    ///
    /// The pre-existing `build_twenty` helper is NOT usable for ordering
    /// tests — see `test_finding_existing_build_twenty_fixture_is_not_a_legal_bid_book`.
    fn build_twenty_realistic(code: u8, sid: u32, best: f64, populated: usize) -> Vec<u8> {
        let total = DEEP_DEPTH_HEADER_SIZE + TWENTY_DEPTH_LEVELS * DEEP_DEPTH_LEVEL_SIZE;
        let mut buf = vec![0u8; total];
        put_header(&mut buf, total as u16, code, 1, sid, 0);
        let descending = code == DEEP_DEPTH_FEED_CODE_BID;
        for i in 0..populated.min(TWENTY_DEPTH_LEVELS) {
            let step = 0.05 * i as f64;
            let price = if descending { best - step } else { best + step };
            put_level(&mut buf, i, price, 10 * (i as u32 + 1), i as u32 + 1);
        }
        buf
    }

    fn parse_twenty_levels<'b>(raw: &[u8], buf: &'b mut DepthLevelBuffer) -> &'b [DeepDepthLevel] {
        let packet = parse_depth_packet(raw, DepthFeedKind::Twenty, buf).unwrap();
        match packet.payload {
            DepthPayload::Levels { levels, .. } => levels,
            DepthPayload::Disconnect { .. } => panic!("expected levels"),
        }
    }

    // ---- the invariants hold on the GOLDEN data ---------------------------

    #[test]
    fn test_golden_twenty_bid_packet_prices_are_non_increasing() {
        // The golden vector's two derived levels are 100.5 then 100.25 —
        // a legal bid book. Levels 2..19 are vendor zero-padding and are
        // excluded as ABSENT, not silently satisfied by `100.25 >= 0.0`.
        let raw = golden_twenty_bid_packet();
        let mut buf = DepthLevelBuffer::new();
        let levels = parse_twenty_levels(&raw, &mut buf);
        assert_eq!(levels.len(), TWENTY_DEPTH_LEVELS);
        check_bid_ordering(levels).unwrap();
        check_populated_levels_are_a_prefix(levels).unwrap();
        // Prove the exclusion is real and not incidental: exactly 2 live.
        assert_eq!(
            levels.iter().filter(|l| !depth_level_is_absent(l)).count(),
            2,
            "golden vector derives 2 levels; the other 18 must read as ABSENT"
        );
    }

    #[test]
    fn test_realistic_twenty_book_is_ordered_and_uncrossed() {
        let bid_raw = build_twenty_realistic(DEEP_DEPTH_FEED_CODE_BID, 1333, 100.50, 20);
        let ask_raw = build_twenty_realistic(DEEP_DEPTH_FEED_CODE_ASK, 1333, 100.55, 20);
        let mut b1 = DepthLevelBuffer::new();
        let mut b2 = DepthLevelBuffer::new();
        let bids: Vec<DeepDepthLevel> = parse_twenty_levels(&bid_raw, &mut b1).to_vec();
        let asks: Vec<DeepDepthLevel> = parse_twenty_levels(&ask_raw, &mut b2).to_vec();
        check_bid_ordering(&bids).unwrap();
        check_ask_ordering(&asks).unwrap();
        check_populated_levels_are_a_prefix(&bids).unwrap();
        check_populated_levels_are_a_prefix(&asks).unwrap();
        check_book_uncrossed(&bids, &asks).unwrap();
    }

    #[test]
    fn test_partially_filled_twenty_book_is_ordered_and_uncrossed() {
        // 3 populated levels + 17 pad levels — the shape the exchange
        // actually sends for a thin instrument.
        let bid_raw = build_twenty_realistic(DEEP_DEPTH_FEED_CODE_BID, 9, 250.00, 3);
        let ask_raw = build_twenty_realistic(DEEP_DEPTH_FEED_CODE_ASK, 9, 250.05, 3);
        let mut b1 = DepthLevelBuffer::new();
        let mut b2 = DepthLevelBuffer::new();
        let bids: Vec<DeepDepthLevel> = parse_twenty_levels(&bid_raw, &mut b1).to_vec();
        let asks: Vec<DeepDepthLevel> = parse_twenty_levels(&ask_raw, &mut b2).to_vec();
        assert_eq!(bids.iter().filter(|l| !depth_level_is_absent(l)).count(), 3);
        check_bid_ordering(&bids).unwrap();
        check_ask_ordering(&asks).unwrap();
        check_populated_levels_are_a_prefix(&bids).unwrap();
        check_book_uncrossed(&bids, &asks).unwrap();
    }

    // ---- the invariants FIRE on violations (bite-proof, permanent) --------
    //
    // A test that cannot fail is worse than no test. Each checker is proven
    // here to REJECT the exact corruption it exists to catch, so the positive
    // assertions above can never degrade into no-ops.

    #[test]
    fn test_bid_ordering_check_rejects_a_swapped_level_pair() {
        let raw = build_twenty_realistic(DEEP_DEPTH_FEED_CODE_BID, 1333, 100.50, 5);
        let mut buf = DepthLevelBuffer::new();
        let mut bids: Vec<DeepDepthLevel> = parse_twenty_levels(&raw, &mut buf).to_vec();
        check_bid_ordering(&bids).unwrap();
        bids.swap(1, 2); // exactly what a level-index drift produces
        let err = check_bid_ordering(&bids).unwrap_err();
        assert!(err.contains("BID ORDERING VIOLATED"), "{err}");
    }

    #[test]
    fn test_ask_ordering_check_rejects_a_swapped_level_pair() {
        let raw = build_twenty_realistic(DEEP_DEPTH_FEED_CODE_ASK, 1333, 100.55, 5);
        let mut buf = DepthLevelBuffer::new();
        let mut asks: Vec<DeepDepthLevel> = parse_twenty_levels(&raw, &mut buf).to_vec();
        check_ask_ordering(&asks).unwrap();
        asks.swap(0, 3);
        let err = check_ask_ordering(&asks).unwrap_err();
        assert!(err.contains("ASK ORDERING VIOLATED"), "{err}");
    }

    #[test]
    fn test_uncrossed_check_rejects_a_crossed_book() {
        // Side-code inversion (the 41/51 hazard the module docs warn about)
        // presents exactly as a crossed book: the "bid" side carries the
        // ask prices and vice versa.
        let bid_raw = build_twenty_realistic(DEEP_DEPTH_FEED_CODE_BID, 1333, 100.50, 5);
        let ask_raw = build_twenty_realistic(DEEP_DEPTH_FEED_CODE_ASK, 1333, 100.55, 5);
        let mut b1 = DepthLevelBuffer::new();
        let mut b2 = DepthLevelBuffer::new();
        let bids: Vec<DeepDepthLevel> = parse_twenty_levels(&bid_raw, &mut b1).to_vec();
        let asks: Vec<DeepDepthLevel> = parse_twenty_levels(&ask_raw, &mut b2).to_vec();
        check_book_uncrossed(&bids, &asks).unwrap();
        // Swap the sides -> best "bid" 100.55 >= best "ask" 100.50.
        let err = check_book_uncrossed(&asks, &bids).unwrap_err();
        assert!(err.contains("BOOK CROSSED"), "{err}");
    }

    #[test]
    fn test_prefix_check_rejects_a_hole_between_populated_levels() {
        let raw = build_twenty_realistic(DEEP_DEPTH_FEED_CODE_BID, 1333, 100.50, 5);
        let mut buf = DepthLevelBuffer::new();
        let mut bids: Vec<DeepDepthLevel> = parse_twenty_levels(&raw, &mut buf).to_vec();
        check_populated_levels_are_a_prefix(&bids).unwrap();
        bids[2] = DeepDepthLevel::default(); // blank a middle level
        let err = check_populated_levels_are_a_prefix(&bids).unwrap_err();
        assert!(err.contains("DEPTH HOLE"), "{err}");
    }

    #[test]
    fn test_absent_level_definition_is_explicit_not_incidental() {
        // If "absent" ever silently widened to include real levels, the
        // ordering checks would pass vacuously by filtering everything out.
        assert!(depth_level_is_absent(&DeepDepthLevel::default()));
        assert!(depth_level_is_absent(&DeepDepthLevel {
            price: 100.5,
            quantity: 0,
            orders: 0
        }));
        assert!(depth_level_is_absent(&DeepDepthLevel {
            price: -1.0,
            quantity: 5,
            orders: 1
        }));
        assert!(depth_level_is_absent(&DeepDepthLevel {
            price: f64::NAN,
            quantity: 5,
            orders: 1
        }));
        assert!(!depth_level_is_absent(&DeepDepthLevel {
            price: 100.5,
            quantity: 250,
            orders: 3
        }));
        // A real level with 0 orders but real size is still PRESENT.
        assert!(!depth_level_is_absent(&DeepDepthLevel {
            price: 100.5,
            quantity: 250,
            orders: 0
        }));
    }

    // ---- FINDING ---------------------------------------------------------

    #[test]
    fn test_finding_existing_build_twenty_fixture_is_not_a_legal_bid_book() {
        // FINDING (2026-08-25, recorded loudly rather than worked around):
        // the pre-existing `build_twenty` helper emits price = 100.0 + i, an
        // ASCENDING ladder, and the suite feeds it through code 41 (BID) in
        // several tests. A real bid book DESCENDS. The fixture is therefore
        // not a legal order book, which is precisely why 46 field-value tests
        // could never have caught an ordering defect: the fixture they assert
        // against is already ordered wrongly for one of the two sides.
        //
        // NOT "fixed" here: `build_twenty` is load-bearing for the existing
        // round-trip and offset tests, which legitimately only care that
        // level i decodes to the value written at level i. Changing it would
        // churn tests that are correct for their own purpose. It is pinned as
        // a FINDING so the next reader knows the fixture is synthetic and
        // must not be reused for book-shape reasoning.
        let raw = build_twenty(DEEP_DEPTH_FEED_CODE_BID, 1, 1333, 0);
        let mut buf = DepthLevelBuffer::new();
        let levels = parse_twenty_levels(&raw, &mut buf);
        let err = check_bid_ordering(levels)
            .expect_err("build_twenty ascends; as a BID book that is a violation");
        assert!(err.contains("BID ORDERING VIOLATED"), "{err}");
        // As an ASK book the same bytes are legal — proving the checkers are
        // side-aware and not merely "sorted somehow".
        check_ask_ordering(levels).unwrap();
    }

    #[test]
    fn test_finding_existing_build_two_hundred_fixture_is_not_a_legal_bid_book() {
        // Same finding on the depth-200 fixture (500.0 + i*0.25, ascending).
        let raw = build_two_hundred(DEEP_DEPTH_FEED_CODE_BID, 2, 7, 200);
        let mut buf = DepthLevelBuffer::new();
        let packet = parse_depth_packet(&raw, DepthFeedKind::TwoHundred, &mut buf).unwrap();
        let (_, levels) = levels_of(&packet);
        assert!(
            check_bid_ordering(levels).is_err(),
            "ascending bid must fail"
        );
        check_ask_ordering(levels).unwrap();
        check_populated_levels_are_a_prefix(levels).unwrap();
    }

    #[test]
    fn test_realistic_two_hundred_book_is_ordered_and_uncrossed() {
        let rows: u32 = 200;
        let build = |code: u8, best: f64| {
            let count = rows as usize;
            let total = DEEP_DEPTH_HEADER_SIZE + count * DEEP_DEPTH_LEVEL_SIZE;
            let mut buf = vec![0u8; total];
            put_header(&mut buf, total as u16, code, 2, 7, rows);
            let descending = code == DEEP_DEPTH_FEED_CODE_BID;
            for i in 0..count {
                let step = 0.05 * i as f64;
                let price = if descending { best - step } else { best + step };
                put_level(&mut buf, i, price, 7 * (i as u32 + 1), i as u32 + 1);
            }
            buf
        };
        let bid_raw = build(DEEP_DEPTH_FEED_CODE_BID, 500.00);
        let ask_raw = build(DEEP_DEPTH_FEED_CODE_ASK, 500.05);
        let mut b1 = DepthLevelBuffer::new();
        let mut b2 = DepthLevelBuffer::new();
        let bids: Vec<DeepDepthLevel> = {
            let p = parse_depth_packet(&bid_raw, DepthFeedKind::TwoHundred, &mut b1).unwrap();
            levels_of(&p).1.to_vec()
        };
        let asks: Vec<DeepDepthLevel> = {
            let p = parse_depth_packet(&ask_raw, DepthFeedKind::TwoHundred, &mut b2).unwrap();
            levels_of(&p).1.to_vec()
        };
        assert_eq!(bids.len(), 200);
        check_bid_ordering(&bids).unwrap();
        check_ask_ordering(&asks).unwrap();
        check_populated_levels_are_a_prefix(&bids).unwrap();
        check_book_uncrossed(&bids, &asks).unwrap();
    }

    // ---- property coverage over ORDERING ---------------------------------

    proptest! {
        /// Any descending bid ladder, at any level count, must satisfy the
        /// bid ordering invariant after a real parse round-trip; the same
        /// bytes routed through the ask checker must be REJECTED whenever
        /// there are at least two distinct populated levels. The second half
        /// is what stops the checker degenerating into "always Ok".
        #[test]
        fn test_bid_ladder_roundtrip_satisfies_ordering_and_rejects_as_ask(
            populated in 2_usize..=20,
            best in 1.0_f64..5000.0,
        ) {
            let raw = build_twenty_realistic(DEEP_DEPTH_FEED_CODE_BID, 1333, best, populated);
            let mut buf = DepthLevelBuffer::new();
            let packet = parse_depth_packet(&raw, DepthFeedKind::Twenty, &mut buf)
                .map_err(|e| TestCaseError::fail(format!("{e:?}")))?;
            let (side, levels) = levels_of(&packet);
            prop_assert_eq!(side, DepthSide::Bid);
            prop_assert!(check_bid_ordering(levels).is_ok(), "descending ladder must pass");
            prop_assert!(check_populated_levels_are_a_prefix(levels).is_ok());
            prop_assert!(
                check_ask_ordering(levels).is_err(),
                "a strictly descending ladder can never be a legal ask book"
            );
        }
    }
}
