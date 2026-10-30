//! Adversarial properties for every Dhan binary frame shape that enters the
//! process from a socket: main-feed packets of every response code (ticker 1/2,
//! quote 4, OI 5, previous close 6, market status 7, full 8, disconnect 50 and
//! unknown codes, including the retired v1 depth code 3), depth-20 and
//! depth-200 packets (12-byte header), STACKED several to a frame, and the
//! socket-side classifier `websocket::connection::classify_frame` for every
//! `DhanEndpointType`.
//!
//! # What is already covered elsewhere (and is NOT repeated here)
//!
//! `tests/proptest_parser.rs` round-trips ONE packet per code and fuzzes
//! `dispatch_frame` for no-panic; `parser::depth`'s own test module round-trips
//! single depth packets and has example tests for stacked depth frames;
//! `parser::dispatcher` has example tests for a disconnect stacked behind data.
//! None of them stacks mixed codes, truncates at every byte, flips every bit,
//! or compares the classifier against an independent oracle.
//!
//! # The reference
//!
//! Every property compares production code against a REFERENCE written in this
//! file from `docs/dhan-ref/03-live-market-feed-websocket.md` and
//! `04-full-market-depth-websocket.md` with literal byte offsets. It does not
//! read `tickvault_common::constants`, so a wrong constant in production and a
//! wrong offset here cannot agree by construction.
//!
//! # Properties
//!
//! 1. `prop_stacked_main_feed_round_trips`: N random valid packets of mixed
//!    codes, concatenated, walked the way the frame drain walks them
//!    (`main_feed_packet_len` then `dispatch_frame` on the exact slice),
//!    reproduce exactly the encoded fields, packet by packet.
//! 2. `prop_unknown_code_stops_the_walk_with_a_typed_error`: an unknown code
//!    anywhere ends the walk there; the packets before it decode, nothing after
//!    it is decoded, the dispatcher refuses the rest with the same typed error
//!    the reference predicts, and no stacked disconnect is reported.
//! 3. `prop_main_feed_truncated_at_every_byte`: cutting a stacked frame at
//!    EVERY offset yields exactly the complete packets before the cut and then
//!    `InsufficientBytes` for the straddling packet; never a fabricated packet.
//!    The stacked-disconnect walk and `classify_frame(MainFeed)` agree with the
//!    model at every cut.
//! 4. `prop_message_length_field_is_dont_care`: the vendor's length stamp at
//!    header bytes 1..3 changes neither the walk nor any decoded field (the
//!    code table is authoritative, by design).
//! 5. `prop_disconnect_stacked_behind_data_is_classified`: a disconnect after
//!    k data packets (and before m more) is found by `classify_frame(MainFeed)`
//!    with its exact reason; the packet cap is exact at the boundary;
//!    `OrderUpdate` never walks; depth endpoints match the depth oracle.
//! 6. `prop_main_feed_bit_flip_is_localised`: flipping any single bit of a
//!    valid packet changes exactly the one decoded field that owns that byte
//!    (or nothing, for the length stamp and for a disconnect's ignored header),
//!    and a flip in the response-code byte yields exactly what the reference
//!    predicts for the new code.
//! 7. `prop_depth_stacked_round_trips` (both kinds): stacked depth packets
//!    split and dispatch back to the encoded fields; the length-stamp mismatch
//!    counter equals the number of lying stamps.
//! 8. `prop_depth_truncated_at_every_byte` (both kinds): `split_depth_frame`
//!    yields only complete packets, reports the exact stop, and the straddling
//!    remainder is refused by `dispatch_depth_packet`; `classify_frame` on the
//!    matching depth endpoint finds a disconnect only in a complete frame.
//! 9. `prop_depth_disconnect_stacked_behind_books_is_classified`.
//! 10. `prop_depth_bit_flip_is_localised` (both kinds).
//! 11. `prop_arbitrary_bytes_match_the_reference_everywhere`: structured and
//!     raw chunks plus random bit flips; `dispatch_frame` on every suffix,
//!     `stacked_disconnect_reason`, `classify_frame` for all four endpoints,
//!     `split_depth_frame` and `dispatch_depth_packet` for both kinds all equal
//!     the reference, field for field. None panics.
//!
//! # Honest limits
//!
//! * The Dhan protocol carries no checksum, so a bit flip inside a value is NOT
//!   refused; property 6 proves it is confined to its own field, which is the
//!   strongest true statement.
//! * The frame drain in `crates/app` additionally skips an unknown code whose
//!   own length stamp lands on a clean header (`unknown_packet_skip`, private
//!   to that crate). This file stops at the unknown code, which is the core
//!   crate's walk (`stacked_disconnect_reason`); the app-side skip is not
//!   exercised here.
//! * `MAX_PACKETS_PER_FRAME` (70,000) itself is not reached; the cap semantics
//!   are pinned at small n by passing an explicit `max_packets`.

use std::collections::BTreeMap;

use proptest::prelude::*;
use tickvault_common::tick_types::ParsedTick;
use tickvault_core::parser::depth::stacked_depth_disconnect_reason;
use tickvault_core::parser::dispatcher::{
    dispatch_frame, main_feed_packet_len, stacked_disconnect_reason,
};
use tickvault_core::parser::{
    DepthFeedKind, DepthLevelBuffer, DepthPacket, DepthPayload, DepthSplitStop, ParseError,
    ParsedFrame, dispatch_depth_packet, split_depth_frame,
};
use tickvault_core::websocket::connection::{FrameClass, classify_frame};
use tickvault_core::websocket::pool_budget::DhanEndpointType;
use tickvault_core::websocket::types::DisconnectCode;

// ===========================================================================
// Reference: main feed (literal offsets from 03-live-market-feed-websocket.md)
// ===========================================================================

const HEADER_LEN: usize = 8;
const MAX_PACKETS: u32 = 70_000;
const KNOWN_CODES: [u8; 8] = [1, 2, 4, 5, 6, 7, 8, 50];
const NAMED_DISCONNECT_REASONS: [u16; 15] = [
    800, 801, 803, 804, 805, 806, 807, 808, 809, 810, 811, 812, 813, 814, 0,
];

/// Packet size by response code, per the protocol tables.
fn ref_size(code: u8) -> Option<usize> {
    match code {
        1 | 2 => Some(16),
        4 => Some(50),
        5 => Some(12),
        6 => Some(16),
        7 => Some(8),
        8 => Some(162),
        50 => Some(10),
        _ => None,
    }
}

/// Field name, byte offset from packet start, width in bytes. Floats are
/// carried as their raw bit pattern so NaN and -0.0 compare exactly.
type FieldSpec = (&'static str, usize, usize);

const DEPTH5_NAMES: [[&str; 6]; 5] = [
    [
        "d0_bid_qty",
        "d0_ask_qty",
        "d0_bid_orders",
        "d0_ask_orders",
        "d0_bid_price",
        "d0_ask_price",
    ],
    [
        "d1_bid_qty",
        "d1_ask_qty",
        "d1_bid_orders",
        "d1_ask_orders",
        "d1_bid_price",
        "d1_ask_price",
    ],
    [
        "d2_bid_qty",
        "d2_ask_qty",
        "d2_bid_orders",
        "d2_ask_orders",
        "d2_bid_price",
        "d2_ask_price",
    ],
    [
        "d3_bid_qty",
        "d3_ask_qty",
        "d3_bid_orders",
        "d3_ask_orders",
        "d3_bid_price",
        "d3_ask_price",
    ],
    [
        "d4_bid_qty",
        "d4_ask_qty",
        "d4_bid_orders",
        "d4_ask_orders",
        "d4_bid_price",
        "d4_ask_price",
    ],
];
/// Offset and width of each depth-level field inside its 20-byte level.
const DEPTH5_LEVEL_FIELDS: [(usize, usize); 6] =
    [(0, 4), (4, 4), (8, 2), (10, 2), (12, 4), (16, 4)];

/// The payload fields (after the 8-byte header) of each known code.
fn payload_layout(code: u8) -> Vec<FieldSpec> {
    match code {
        1 | 2 => vec![("ltp", 8, 4), ("ltt", 12, 4)],
        4 => vec![
            ("ltp", 8, 4),
            ("ltq", 12, 2),
            ("ltt", 14, 4),
            ("atp", 18, 4),
            ("volume", 22, 4),
            ("sell", 26, 4),
            ("buy", 30, 4),
            ("open", 34, 4),
            ("close", 38, 4),
            ("high", 42, 4),
            ("low", 46, 4),
        ],
        8 => {
            let mut v = vec![
                ("ltp", 8, 4),
                ("ltq", 12, 2),
                ("ltt", 14, 4),
                ("atp", 18, 4),
                ("volume", 22, 4),
                ("sell", 26, 4),
                ("buy", 30, 4),
                ("oi", 34, 4),
                ("oih", 38, 4),
                ("oil", 42, 4),
                ("open", 46, 4),
                ("close", 50, 4),
                ("high", 54, 4),
                ("low", 58, 4),
            ];
            for (level, names) in DEPTH5_NAMES.iter().enumerate() {
                let base = 62 + level * 20;
                for (name, (off, width)) in names.iter().zip(DEPTH5_LEVEL_FIELDS) {
                    v.push((name, base + off, width));
                }
            }
            v
        }
        5 => vec![("oi", 8, 4)],
        6 => vec![("prev_close", 8, 4), ("prev_oi", 12, 4)],
        50 => vec![("disconnect_reason", 8, 2)],
        _ => Vec::new(),
    }
}

fn mask(width: usize) -> u64 {
    match width {
        1 => 0xff,
        2 => 0xffff,
        4 => 0xffff_ffff,
        _ => u64::MAX,
    }
}

fn read_le(bytes: &[u8], off: usize, width: usize) -> u64 {
    let mut v = 0u64;
    for i in 0..width {
        v |= u64::from(bytes[off + i]) << (8 * i);
    }
    v
}

fn write_le(bytes: &mut [u8], off: usize, width: usize, value: u64) {
    for i in 0..width {
        bytes[off + i] = (value >> (8 * i)) as u8;
    }
}

/// A decoded frame in canonical, comparable form: kind + every field.
type Canon = (&'static str, BTreeMap<&'static str, u64>);

/// A parse error in comparable form (ParseError has no PartialEq).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RefErr {
    Insufficient { expected: usize, actual: usize },
    Unknown(u8),
    RowCount { actual: u32, max: usize },
}

fn err_canon(e: &ParseError) -> RefErr {
    match *e {
        ParseError::InsufficientBytes { expected, actual } => {
            RefErr::Insufficient { expected, actual }
        }
        ParseError::UnknownResponseCode(c) => RefErr::Unknown(c),
        ParseError::InvalidRowCount { actual, max } => RefErr::RowCount { actual, max },
    }
}

fn tick_fields(t: &ParsedTick) -> BTreeMap<&'static str, u64> {
    BTreeMap::from([
        ("segment", u64::from(t.exchange_segment_code)),
        ("security_id", t.security_id),
        ("ltp", u64::from(t.last_traded_price.to_bits())),
        ("ltq", u64::from(t.last_trade_quantity)),
        ("ltt", u64::from(t.exchange_timestamp)),
        ("received_at", t.received_at_nanos as u64),
        ("atp", u64::from(t.average_traded_price.to_bits())),
        ("volume", u64::from(t.volume)),
        ("sell", u64::from(t.total_sell_quantity)),
        ("buy", u64::from(t.total_buy_quantity)),
        ("open", u64::from(t.day_open.to_bits())),
        ("close", u64::from(t.day_close.to_bits())),
        ("high", u64::from(t.day_high.to_bits())),
        ("low", u64::from(t.day_low.to_bits())),
        ("oi", u64::from(t.open_interest)),
        ("oih", u64::from(t.oi_day_high)),
        ("oil", u64::from(t.oi_day_low)),
        ("iv", t.iv.to_bits()),
        ("delta", t.delta.to_bits()),
        ("gamma", t.gamma.to_bits()),
        ("theta", t.theta.to_bits()),
        ("vega", t.vega.to_bits()),
    ])
}

/// Canonical form of what production returned.
fn canon_of(frame: &ParsedFrame) -> Canon {
    match frame {
        ParsedFrame::Tick(t) => ("tick", tick_fields(t)),
        ParsedFrame::TickWithDepth(t, depth) => {
            let mut m = tick_fields(t);
            for (level, names) in depth.iter().zip(DEPTH5_NAMES) {
                m.insert(names[0], u64::from(level.bid_quantity));
                m.insert(names[1], u64::from(level.ask_quantity));
                m.insert(names[2], u64::from(level.bid_orders));
                m.insert(names[3], u64::from(level.ask_orders));
                m.insert(names[4], u64::from(level.bid_price.to_bits()));
                m.insert(names[5], u64::from(level.ask_price.to_bits()));
            }
            ("tick_with_depth", m)
        }
        ParsedFrame::OiUpdate {
            security_id,
            exchange_segment_code,
            open_interest,
        } => (
            "oi",
            BTreeMap::from([
                ("segment", u64::from(*exchange_segment_code)),
                ("security_id", *security_id),
                ("oi", u64::from(*open_interest)),
            ]),
        ),
        ParsedFrame::PreviousClose {
            security_id,
            exchange_segment_code,
            previous_close,
            previous_oi,
        } => (
            "prev_close",
            BTreeMap::from([
                ("segment", u64::from(*exchange_segment_code)),
                ("security_id", *security_id),
                ("prev_close", u64::from(previous_close.to_bits())),
                ("prev_oi", u64::from(*previous_oi)),
            ]),
        ),
        ParsedFrame::MarketStatus {
            security_id,
            exchange_segment_code,
        } => (
            "market_status",
            BTreeMap::from([
                ("segment", u64::from(*exchange_segment_code)),
                ("security_id", *security_id),
            ]),
        ),
        ParsedFrame::Disconnect(code) => (
            "disconnect",
            BTreeMap::from([("disconnect_reason", u64::from(code.as_u16()))]),
        ),
    }
}

fn kind_of(code: u8) -> &'static str {
    match code {
        1 | 2 | 4 => "tick",
        8 => "tick_with_depth",
        5 => "oi",
        6 => "prev_close",
        7 => "market_status",
        _ => "disconnect",
    }
}

/// Reference decode of one packet starting at `b[0]` (trailing bytes ignored,
/// exactly as the per-code parsers only check a minimum length).
fn ref_decode(b: &[u8], recv: i64) -> Result<Canon, RefErr> {
    if b.len() < HEADER_LEN {
        return Err(RefErr::Insufficient {
            expected: HEADER_LEN,
            actual: b.len(),
        });
    }
    let code = b[0];
    let size = ref_size(code).ok_or(RefErr::Unknown(code))?;
    if b.len() < size {
        return Err(RefErr::Insufficient {
            expected: size,
            actual: b.len(),
        });
    }
    let kind = kind_of(code);
    let mut m = if kind.starts_with("tick") {
        let mut m = tick_fields(&ParsedTick::default());
        m.insert("received_at", recv as u64);
        m
    } else {
        BTreeMap::new()
    };
    if kind != "disconnect" {
        m.insert("segment", read_le(b, 3, 1));
        m.insert("security_id", read_le(b, 4, 4));
    }
    for (name, off, width) in payload_layout(code) {
        m.insert(name, read_le(b, off, width));
    }
    Ok((kind, m))
}

/// The field a byte of a packet of `code` belongs to in the decoded output.
/// `None` = the decoder does not read that byte into any output field.
fn field_at(code: u8, byte: usize) -> Option<&'static str> {
    if code != 50 {
        if byte == 3 {
            return Some("segment");
        }
        if (4..8).contains(&byte) {
            return Some("security_id");
        }
    }
    payload_layout(code)
        .into_iter()
        .find(|&(_, off, width)| (off..off + width).contains(&byte))
        .map(|(name, _, _)| name)
}

/// Reference: the first disconnect reason in a frame that walks end to end by
/// the protocol size table, else `None`.
fn ref_main_stacked(frame: &[u8]) -> Option<u16> {
    let mut offset = 0usize;
    let mut found = None;
    while offset < frame.len() {
        let size = ref_size(frame[offset])?;
        if offset + size > frame.len() {
            return None;
        }
        if found.is_none() && frame[offset] == 50 {
            found = Some(read_le(frame, offset + 8, 2) as u16);
        }
        offset += size;
    }
    found
}

// ===========================================================================
// Reference: depth feeds (literal offsets from 04-full-market-depth-websocket)
// ===========================================================================

const DEPTH_HEADER_LEN: usize = 12;

fn kind_level_count(kind: DepthFeedKind, rows: u32) -> Result<usize, RefErr> {
    match kind {
        DepthFeedKind::Twenty => Ok(20),
        DepthFeedKind::TwoHundred => {
            if rows > 200 {
                Err(RefErr::RowCount {
                    actual: rows,
                    max: 200,
                })
            } else {
                Ok(rows as usize)
            }
        }
    }
}

/// Size of the depth packet at `b[0..]` (`b.len() >= 12`).
fn ref_depth_size(b: &[u8], kind: DepthFeedKind) -> Result<usize, RefErr> {
    let code = b[2];
    if code == 50 {
        return Ok(14);
    }
    if code != 41 && code != 51 {
        return Err(RefErr::Unknown(code));
    }
    let rows = read_le(b, 8, 4) as u32;
    Ok(12 + 16 * kind_level_count(kind, rows)?)
}

type DepthCanon = BTreeMap<(&'static str, u32), u64>;

fn depth_canon_of(p: &DepthPacket<'_>) -> DepthCanon {
    let h = &p.header;
    let mut m = DepthCanon::from([
        (("msg_len", 0), u64::from(h.message_length)),
        (("code", 0), u64::from(h.response_code)),
        (("segment", 0), u64::from(h.exchange_segment_code)),
        (("security_id", 0), h.security_id),
        (("seq", 0), u64::from(h.sequence_or_row_count)),
    ]);
    match p.payload {
        DepthPayload::Levels { side, levels } => {
            m.insert(("side", 0), u64::from(side.as_feed_code()));
            m.insert(("level_count", 0), levels.len() as u64);
            for (i, l) in levels.iter().enumerate() {
                let i = i as u32;
                m.insert(("price", i), l.price.to_bits());
                m.insert(("qty", i), u64::from(l.quantity));
                m.insert(("orders", i), u64::from(l.orders));
            }
        }
        DepthPayload::Disconnect { reason_code } => {
            m.insert(("reason", 0), u64::from(reason_code));
        }
    }
    m
}

fn ref_depth_decode(b: &[u8], kind: DepthFeedKind) -> Result<DepthCanon, RefErr> {
    if b.len() < DEPTH_HEADER_LEN {
        return Err(RefErr::Insufficient {
            expected: DEPTH_HEADER_LEN,
            actual: b.len(),
        });
    }
    let size = ref_depth_size(b, kind)?;
    if b.len() < size {
        return Err(RefErr::Insufficient {
            expected: size,
            actual: b.len(),
        });
    }
    let mut m = DepthCanon::from([
        (("msg_len", 0), read_le(b, 0, 2)),
        (("code", 0), u64::from(b[2])),
        (("segment", 0), u64::from(b[3])),
        (("security_id", 0), read_le(b, 4, 4)),
        (("seq", 0), read_le(b, 8, 4)),
    ]);
    if b[2] == 50 {
        m.insert(("reason", 0), read_le(b, 12, 2));
    } else {
        let n = (size - 12) / 16;
        m.insert(("side", 0), u64::from(b[2]));
        m.insert(("level_count", 0), n as u64);
        for i in 0..n {
            let base = 12 + 16 * i;
            m.insert(("price", i as u32), read_le(b, base, 8));
            m.insert(("qty", i as u32), read_le(b, base + 8, 4));
            m.insert(("orders", i as u32), read_le(b, base + 12, 4));
        }
    }
    Ok(m)
}

/// Reference splitter: yielded `(offset, len)`, the stop reason, and the
/// count of packets whose length stamp disagreed with the derived size.
fn ref_depth_split(
    frame: &[u8],
    kind: DepthFeedKind,
) -> (Vec<(usize, usize)>, DepthSplitStop, u32) {
    let mut out = Vec::new();
    let mut mismatches = 0u32;
    let mut offset = 0usize;
    loop {
        let rest = &frame[offset..];
        if rest.is_empty() {
            return (out, DepthSplitStop::Complete, mismatches);
        }
        if rest.len() < DEPTH_HEADER_LEN {
            return (
                out,
                DepthSplitStop::TruncatedHeader {
                    remaining: rest.len(),
                },
                mismatches,
            );
        }
        let size = match ref_depth_size(rest, kind) {
            Ok(s) => s,
            Err(RefErr::Unknown(c)) => {
                return (out, DepthSplitStop::UnknownResponseCode(c), mismatches);
            }
            Err(RefErr::RowCount { actual, .. }) => {
                return (out, DepthSplitStop::InvalidRowCount(actual), mismatches);
            }
            Err(RefErr::Insufficient { .. }) => unreachable!("size never reports length"),
        };
        if read_le(rest, 0, 2) as usize != size {
            mismatches += 1;
        }
        if rest.len() < size {
            return (
                out,
                DepthSplitStop::TruncatedBody {
                    expected: size,
                    actual: rest.len(),
                },
                mismatches,
            );
        }
        out.push((offset, size));
        offset += size;
    }
}

fn ref_depth_stacked(frame: &[u8], kind: DepthFeedKind) -> Option<u16> {
    let (packets, stop, _) = ref_depth_split(frame, kind);
    if stop != DepthSplitStop::Complete {
        return None;
    }
    packets
        .iter()
        .find(|&&(off, _)| frame[off + 2] == 50)
        .map(|&(off, _)| read_le(frame, off + 12, 2) as u16)
}

/// Independent oracle for `classify_frame`.
fn oracle_classify(endpoint: DhanEndpointType, f: &[u8]) -> FrameClass {
    let disc = |r: u16| FrameClass::Disconnect(DisconnectCode::from_u16(r));
    match endpoint {
        DhanEndpointType::OrderUpdate => {
            if f.len() == 10 && f[0] == 50 {
                disc(read_le(f, 8, 2) as u16)
            } else {
                FrameClass::Data
            }
        }
        DhanEndpointType::MainFeed => {
            if f.len() == 10 {
                if f[0] == 50 {
                    disc(read_le(f, 8, 2) as u16)
                } else {
                    FrameClass::Data
                }
            } else {
                ref_main_stacked(f).map_or(FrameClass::Data, disc)
            }
        }
        DhanEndpointType::Depth20 | DhanEndpointType::Depth200 => {
            let kind = if endpoint == DhanEndpointType::Depth20 {
                DepthFeedKind::Twenty
            } else {
                DepthFeedKind::TwoHundred
            };
            if f.len() == 14 {
                if f[2] == 50 {
                    disc(read_le(f, 12, 2) as u16)
                } else {
                    FrameClass::Data
                }
            } else {
                ref_depth_stacked(f, kind).map_or(FrameClass::Data, disc)
            }
        }
    }
}

// ===========================================================================
// Builders (generated values -> bytes) and the production walk
// ===========================================================================

#[derive(Debug, Clone)]
struct GenPkt {
    code: u8,
    segment: u8,
    security_id: u32,
    /// `None` = stamp the true size; `Some(x)` = a lying length stamp.
    stamp: Option<u16>,
    values: Vec<u64>,
}

impl GenPkt {
    fn size(&self) -> usize {
        ref_size(self.code).expect("known code")
    }

    fn encode(&self) -> Vec<u8> {
        let size = self.size();
        let mut b = vec![0u8; size];
        b[0] = self.code;
        write_le(&mut b, 1, 2, u64::from(self.stamp.unwrap_or(size as u16)));
        b[3] = self.segment;
        write_le(&mut b, 4, 4, u64::from(self.security_id));
        for ((_, off, width), v) in payload_layout(self.code).into_iter().zip(&self.values) {
            write_le(&mut b, off, width, *v);
        }
        b
    }

    /// What the builder put in, in canonical form: the round-trip reference.
    fn expected(&self, recv: i64) -> Canon {
        let kind = kind_of(self.code);
        let mut m = if kind.starts_with("tick") {
            let mut m = tick_fields(&ParsedTick::default());
            m.insert("received_at", recv as u64);
            m
        } else {
            BTreeMap::new()
        };
        if kind != "disconnect" {
            m.insert("segment", u64::from(self.segment));
            m.insert("security_id", u64::from(self.security_id));
        }
        for ((name, _, width), v) in payload_layout(self.code).into_iter().zip(&self.values) {
            m.insert(name, v & mask(width));
        }
        (kind, m)
    }
}

fn gen_pkt_with(codes: Vec<u8>) -> impl Strategy<Value = GenPkt> {
    (
        prop::sample::select(codes),
        any::<u8>(),
        any::<u32>(),
        prop::option::weighted(0.25, any::<u16>()),
        prop::collection::vec(any::<u64>(), 46),
        prop_oneof![
            prop::sample::select(NAMED_DISCONNECT_REASONS.to_vec()),
            any::<u16>()
        ],
    )
        .prop_map(|(code, segment, security_id, stamp, mut values, reason)| {
            if code == 50 {
                values[0] = u64::from(reason);
            }
            GenPkt {
                code,
                segment,
                security_id,
                stamp,
                values,
            }
        })
}

fn gen_pkt() -> impl Strategy<Value = GenPkt> {
    gen_pkt_with(KNOWN_CODES.to_vec())
}

fn gen_data_pkt() -> impl Strategy<Value = GenPkt> {
    gen_pkt_with(KNOWN_CODES.iter().copied().filter(|&c| c != 50).collect())
}

/// An unknown-code packet: any code outside the main-feed table (3, the
/// retired v1 depth code, included) followed by 0..40 arbitrary bytes.
fn gen_unknown_pkt() -> impl Strategy<Value = Vec<u8>> {
    (
        any::<u8>().prop_filter("unknown code", |c| !KNOWN_CODES.contains(c)),
        prop::collection::vec(any::<u8>(), 0..40),
    )
        .prop_map(|(code, mut body)| {
            body.insert(0, code);
            body
        })
}

fn stack(pkts: &[GenPkt]) -> (Vec<u8>, Vec<(usize, usize)>) {
    let mut frame = Vec::new();
    let mut bounds = Vec::new();
    for p in pkts {
        let b = p.encode();
        bounds.push((frame.len(), b.len()));
        frame.extend_from_slice(&b);
    }
    (frame, bounds)
}

#[derive(Debug, PartialEq)]
enum WalkStop {
    Complete,
    Partial { offset: usize, err: RefErr },
    Unknown { offset: usize, err: RefErr },
}

#[derive(Debug, PartialEq)]
struct Walk {
    decoded: Vec<(usize, usize, Canon)>,
    stop: WalkStop,
}

/// The frame drain's walk: size from `main_feed_packet_len`, decode the exact
/// slice with `dispatch_frame`. Panics (failing the property) when the length
/// table and the dispatcher disagree about what a packet is.
fn walk(frame: &[u8], recv: i64) -> Walk {
    let mut offset = 0usize;
    let mut decoded = Vec::new();
    loop {
        if offset == frame.len() {
            return Walk {
                decoded,
                stop: WalkStop::Complete,
            };
        }
        let rest = &frame[offset..];
        match main_feed_packet_len(rest) {
            None => {
                let err = match dispatch_frame(rest, recv) {
                    Err(e) => err_canon(&e),
                    Ok(f) => panic!(
                        "length table says unknown code {} but dispatch_frame decoded {f:?}",
                        rest[0]
                    ),
                };
                return Walk {
                    decoded,
                    stop: WalkStop::Unknown { offset, err },
                };
            }
            Some(len) => {
                assert_eq!(
                    Some(len),
                    ref_size(rest[0]),
                    "length table disagrees with the protocol for code {}",
                    rest[0]
                );
                if offset + len > frame.len() {
                    let err = match dispatch_frame(rest, recv) {
                        Err(e) => err_canon(&e),
                        Ok(f) => panic!(
                            "dispatch_frame fabricated {f:?} from a {}-byte remainder of a \
                             {len}-byte packet",
                            rest.len()
                        ),
                    };
                    return Walk {
                        decoded,
                        stop: WalkStop::Partial { offset, err },
                    };
                }
                let parsed = dispatch_frame(&frame[offset..offset + len], recv)
                    .unwrap_or_else(|e| panic!("complete packet refused: {e:?}"));
                decoded.push((offset, len, canon_of(&parsed)));
                offset += len;
            }
        }
    }
}

// ===========================================================================
// Depth builders
// ===========================================================================

#[derive(Debug, Clone)]
struct DGen {
    code: u8,
    segment: u8,
    security_id: u32,
    seq: u32,
    stamp: Option<u16>,
    levels: Vec<(u64, u32, u32)>,
    reason: u16,
}

impl DGen {
    fn size(&self) -> usize {
        if self.code == 50 {
            14
        } else {
            12 + 16 * self.levels.len()
        }
    }

    fn encode(&self) -> Vec<u8> {
        let size = self.size();
        let mut b = vec![0u8; size];
        write_le(&mut b, 0, 2, u64::from(self.stamp.unwrap_or(size as u16)));
        b[2] = self.code;
        b[3] = self.segment;
        write_le(&mut b, 4, 4, u64::from(self.security_id));
        write_le(&mut b, 8, 4, u64::from(self.seq));
        if self.code == 50 {
            write_le(&mut b, 12, 2, u64::from(self.reason));
        } else {
            for (i, &(price, qty, orders)) in self.levels.iter().enumerate() {
                let base = 12 + 16 * i;
                write_le(&mut b, base, 8, price);
                write_le(&mut b, base + 8, 4, u64::from(qty));
                write_le(&mut b, base + 12, 4, u64::from(orders));
            }
        }
        b
    }

    fn expected(&self) -> DepthCanon {
        let mut m = DepthCanon::from([
            (
                ("msg_len", 0),
                u64::from(self.stamp.unwrap_or(self.size() as u16)),
            ),
            (("code", 0), u64::from(self.code)),
            (("segment", 0), u64::from(self.segment)),
            (("security_id", 0), u64::from(self.security_id)),
            (("seq", 0), u64::from(self.seq)),
        ]);
        if self.code == 50 {
            m.insert(("reason", 0), u64::from(self.reason));
        } else {
            m.insert(("side", 0), u64::from(self.code));
            m.insert(("level_count", 0), self.levels.len() as u64);
            for (i, &(price, qty, orders)) in self.levels.iter().enumerate() {
                let i = i as u32;
                m.insert(("price", i), price);
                m.insert(("qty", i), u64::from(qty));
                m.insert(("orders", i), u64::from(orders));
            }
        }
        m
    }
}

fn gen_level() -> impl Strategy<Value = (u64, u32, u32)> {
    // Prices as raw f64 bits: NaN, infinities, subnormals and -0.0 included.
    (any::<u64>(), any::<u32>(), any::<u32>())
}

/// A depth packet for `kind`. `rows` bounds the depth-200 level count so the
/// every-offset sweeps stay fast; the 195..=200 band keeps the cap edge live.
fn gen_depth_pkt(
    kind: DepthFeedKind,
    codes: Vec<u8>,
    rows: BoxedStrategy<u32>,
) -> impl Strategy<Value = DGen> {
    (prop::sample::select(codes), rows).prop_flat_map(move |(code, rows)| {
        let n = match (code, kind) {
            (50, _) => 0usize,
            (_, DepthFeedKind::Twenty) => 20,
            (_, DepthFeedKind::TwoHundred) => rows as usize,
        };
        (
            Just(code),
            Just(rows),
            any::<u8>(),
            any::<u32>(),
            any::<u32>(),
            prop::option::weighted(0.25, any::<u16>()),
            prop::collection::vec(gen_level(), n),
            prop_oneof![
                prop::sample::select(NAMED_DISCONNECT_REASONS.to_vec()),
                any::<u16>()
            ],
        )
            .prop_map(
                move |(code, rows, segment, security_id, seq, stamp, levels, reason)| DGen {
                    code,
                    segment,
                    security_id,
                    // Depth-200 header bytes 8..12 ARE the row count; depth-20
                    // carries a free sequence number there.
                    seq: if kind == DepthFeedKind::TwoHundred && code != 50 {
                        rows
                    } else {
                        seq
                    },
                    stamp,
                    levels,
                    reason,
                },
            )
    })
}

fn small_rows() -> BoxedStrategy<u32> {
    prop_oneof![0u32..=6, 195u32..=200].boxed()
}

fn tiny_rows() -> BoxedStrategy<u32> {
    (0u32..=12).boxed()
}

fn kinds() -> impl Strategy<Value = DepthFeedKind> {
    prop::sample::select(vec![DepthFeedKind::Twenty, DepthFeedKind::TwoHundred])
}

fn endpoint_for(kind: DepthFeedKind) -> DhanEndpointType {
    match kind {
        DepthFeedKind::Twenty => DhanEndpointType::Depth20,
        DepthFeedKind::TwoHundred => DhanEndpointType::Depth200,
    }
}

fn depth_stack(pkts: &[DGen]) -> (Vec<u8>, Vec<(usize, usize)>) {
    let mut frame = Vec::new();
    let mut bounds = Vec::new();
    for p in pkts {
        let b = p.encode();
        bounds.push((frame.len(), b.len()));
        frame.extend_from_slice(&b);
    }
    (frame, bounds)
}

/// Production split as `(offset, len)` relative to the frame start.
fn prod_split(frame: &[u8], kind: DepthFeedKind) -> (Vec<(usize, usize)>, DepthSplitStop, u32) {
    let base = frame.as_ptr() as usize;
    let mut iter = split_depth_frame(frame, kind);
    let mut out = Vec::new();
    for pkt in iter.by_ref() {
        out.push((pkt.as_ptr() as usize - base, pkt.len()));
        assert!(
            out.len() <= frame.len() / DEPTH_HEADER_LEN + 1,
            "non-advancing split"
        );
    }
    let stop = iter
        .stop_reason()
        .expect("exhausted iterator records a stop");
    (out, stop, iter.length_field_mismatches())
}

fn depth_dispatch(raw: &[u8], kind: DepthFeedKind) -> Result<DepthCanon, RefErr> {
    let mut buf = DepthLevelBuffer::new();
    dispatch_depth_packet(raw, kind, &mut buf)
        .map(|p| depth_canon_of(&p))
        .map_err(|e| err_canon(&e))
}

fn diff_keys<K: Ord + Copy>(a: &BTreeMap<K, u64>, b: &BTreeMap<K, u64>) -> Vec<K> {
    let mut keys: Vec<K> = a.keys().chain(b.keys()).copied().collect();
    keys.sort();
    keys.dedup();
    keys.into_iter().filter(|k| a.get(k) != b.get(k)).collect()
}

// ===========================================================================
// Main-feed properties
// ===========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn prop_stacked_main_feed_round_trips(
        pkts in prop::collection::vec(gen_pkt(), 1..12),
        recv in any::<i64>(),
    ) {
        let (frame, bounds) = stack(&pkts);
        let w = walk(&frame, recv);
        prop_assert_eq!(&w.stop, &WalkStop::Complete);
        prop_assert_eq!(w.decoded.len(), pkts.len());
        for ((off, len, canon), (p, &(b_off, b_len))) in
            w.decoded.iter().zip(pkts.iter().zip(&bounds))
        {
            prop_assert_eq!((*off, *len), (b_off, b_len));
            prop_assert_eq!(canon, &p.expected(recv));
        }
        // The core walk used by the classifier agrees with the model.
        let first_disc = pkts.iter().find(|p| p.code == 50).map(|p| p.values[0] as u16);
        prop_assert_eq!(stacked_disconnect_reason(&frame, MAX_PACKETS), first_disc);
    }

    #[test]
    fn prop_unknown_code_stops_the_walk_with_a_typed_error(
        pkts in prop::collection::vec(gen_pkt(), 0..8),
        tail in prop::collection::vec(gen_pkt(), 0..4),
        unknown in gen_unknown_pkt(),
        recv in any::<i64>(),
    ) {
        let (mut frame, bounds) = stack(&pkts);
        let unknown_at = frame.len();
        frame.extend_from_slice(&unknown);
        let (tail_bytes, _) = stack(&tail);
        frame.extend_from_slice(&tail_bytes);

        let w = walk(&frame, recv);
        prop_assert_eq!(w.decoded.len(), pkts.len(), "nothing past the unknown code decodes");
        for ((off, len, canon), (p, &(b_off, b_len))) in
            w.decoded.iter().zip(pkts.iter().zip(&bounds))
        {
            prop_assert_eq!((*off, *len), (b_off, b_len));
            prop_assert_eq!(canon, &p.expected(recv));
        }
        let expected_err = ref_decode(&frame[unknown_at..], recv).expect_err("unknown code refused");
        prop_assert_eq!(
            &w.stop,
            &WalkStop::Unknown { offset: unknown_at, err: expected_err }
        );
        // A frame that does not walk end to end never reports a disconnect,
        // even when a real one sits ahead of the unknown packet.
        prop_assert_eq!(stacked_disconnect_reason(&frame, MAX_PACKETS), None);
        prop_assert_eq!(
            classify_frame(DhanEndpointType::MainFeed, &frame),
            oracle_classify(DhanEndpointType::MainFeed, &frame)
        );
    }

    #[test]
    fn prop_main_feed_truncated_at_every_byte(
        pkts in prop::collection::vec(gen_pkt(), 1..8),
        recv in any::<i64>(),
    ) {
        let (frame, bounds) = stack(&pkts);
        for cut in 0..=frame.len() {
            let sub = &frame[..cut];
            let w = walk(sub, recv);
            let complete = bounds.iter().take_while(|&&(o, l)| o + l <= cut).count();
            prop_assert_eq!(w.decoded.len(), complete, "cut {}", cut);
            for ((off, len, canon), (p, &(b_off, b_len))) in
                w.decoded.iter().zip(pkts.iter().zip(&bounds))
            {
                prop_assert_eq!((*off, *len), (b_off, b_len));
                prop_assert_eq!(canon, &p.expected(recv));
            }
            let on_boundary = complete == pkts.len() || bounds[complete].0 == cut;
            if on_boundary {
                prop_assert_eq!(&w.stop, &WalkStop::Complete, "cut {}", cut);
            } else {
                let (start, size) = bounds[complete];
                let actual = cut - start;
                let expected = if actual < HEADER_LEN { HEADER_LEN } else { size };
                prop_assert_eq!(
                    &w.stop,
                    &WalkStop::Partial {
                        offset: start,
                        err: RefErr::Insufficient { expected, actual },
                    },
                    "cut {}", cut
                );
            }
            let model_disc = if on_boundary {
                pkts[..complete].iter().find(|p| p.code == 50).map(|p| p.values[0] as u16)
            } else {
                None
            };
            prop_assert_eq!(stacked_disconnect_reason(sub, MAX_PACKETS), model_disc, "cut {}", cut);
            let model_class = model_disc.map_or(FrameClass::Data, |r| {
                FrameClass::Disconnect(DisconnectCode::from_u16(r))
            });
            prop_assert_eq!(classify_frame(DhanEndpointType::MainFeed, sub), model_class, "cut {}", cut);
        }
    }

    #[test]
    fn prop_message_length_field_is_dont_care(
        pkts in prop::collection::vec(gen_pkt(), 1..10),
        lies in prop::collection::vec(any::<u16>(), 10),
        recv in any::<i64>(),
    ) {
        let honest: Vec<GenPkt> = pkts.iter().cloned().map(|mut p| { p.stamp = None; p }).collect();
        let lying: Vec<GenPkt> = pkts
            .iter()
            .cloned()
            .zip(&lies)
            .map(|(mut p, &l)| { p.stamp = Some(l); p })
            .collect();
        let (a, _) = stack(&honest);
        let (b, _) = stack(&lying);
        prop_assert_eq!(walk(&a, recv), walk(&b, recv));
        prop_assert_eq!(
            stacked_disconnect_reason(&a, MAX_PACKETS),
            stacked_disconnect_reason(&b, MAX_PACKETS)
        );
        prop_assert_eq!(
            classify_frame(DhanEndpointType::MainFeed, &a),
            classify_frame(DhanEndpointType::MainFeed, &b)
        );
    }

    #[test]
    fn prop_disconnect_stacked_behind_data_is_classified(
        before in prop::collection::vec(gen_data_pkt(), 0..6),
        disc in gen_pkt_with(vec![50]),
        after in prop::collection::vec(gen_pkt(), 0..4),
    ) {
        let mut all = before.clone();
        all.push(disc.clone());
        all.extend(after.iter().cloned());
        let (frame, _) = stack(&all);
        let reason = disc.values[0] as u16;
        let n = all.len() as u32;

        let class = classify_frame(DhanEndpointType::MainFeed, &frame);
        prop_assert_eq!(class, FrameClass::Disconnect(DisconnectCode::from_u16(reason)));
        if let FrameClass::Disconnect(code) = class {
            prop_assert_eq!(code.as_u16(), reason, "reason survives classification");
        }
        // Packet cap: a frame of exactly n packets walks whole at cap n, and
        // is refused (None, i.e. Data) at cap n - 1.
        prop_assert_eq!(stacked_disconnect_reason(&frame, n), Some(reason));
        prop_assert_eq!(stacked_disconnect_reason(&frame, n - 1), None);
        // The order-update endpoint never walks stacked packets.
        prop_assert_eq!(
            classify_frame(DhanEndpointType::OrderUpdate, &frame),
            oracle_classify(DhanEndpointType::OrderUpdate, &frame)
        );
        // Main-feed bytes on a depth endpoint: exactly what the depth oracle says.
        for ep in [DhanEndpointType::Depth20, DhanEndpointType::Depth200] {
            prop_assert_eq!(classify_frame(ep, &frame), oracle_classify(ep, &frame));
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn prop_main_feed_bit_flip_is_localised(pkt in gen_pkt(), recv in any::<i64>()) {
        let mut p = pkt;
        p.stamp = None;
        let bytes = p.encode();
        let orig = canon_of(&dispatch_frame(&bytes, recv).expect("valid packet"));
        prop_assert_eq!(&orig, &p.expected(recv));
        for byte in 0..bytes.len() {
            for bit in 0..8 {
                let mut flipped = bytes.clone();
                flipped[byte] ^= 1 << bit;
                let got = dispatch_frame(&flipped, recv).map(|f| canon_of(&f)).map_err(|e| err_canon(&e));
                match byte {
                    // Response code: no checksum exists, so a flip either
                    // names another code (reinterpreted, exactly as the
                    // reference reads it) or is refused with a typed error.
                    0 => prop_assert_eq!(got, ref_decode(&flipped, recv), "byte 0 bit {}", bit),
                    // Length stamp: never read into any decoded field.
                    1 | 2 => prop_assert_eq!(got, Ok(orig.clone()), "stamp byte {} bit {}", byte, bit),
                    _ => {
                        let got = got.expect("payload flip keeps the packet decodable");
                        prop_assert_eq!(got.0, orig.0);
                        let changed = diff_keys(&orig.1, &got.1);
                        let owner: Vec<&'static str> = field_at(p.code, byte).into_iter().collect();
                        prop_assert_eq!(
                            changed, owner,
                            "code {} byte {} bit {}: a flip must touch only its own field",
                            p.code, byte, bit
                        );
                    }
                }
            }
        }
    }
}

// ===========================================================================
// Depth properties
// ===========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn prop_depth_stacked_round_trips(
        (kind, pkts) in kinds().prop_flat_map(|kind| {
            (Just(kind), prop::collection::vec(gen_depth_pkt(kind, vec![41, 51, 50], small_rows()), 1..6))
        }),
    ) {
        let (frame, bounds) = depth_stack(&pkts);
        let (split, stop, mismatches) = prod_split(&frame, kind);
        prop_assert_eq!(&split, &bounds);
        prop_assert_eq!(stop, DepthSplitStop::Complete);
        let lies = pkts
            .iter()
            .filter(|p| p.stamp.is_some_and(|s| usize::from(s) != p.size()))
            .count() as u32;
        prop_assert_eq!(mismatches, lies);
        for (p, &(off, len)) in pkts.iter().zip(&split) {
            prop_assert_eq!(depth_dispatch(&frame[off..off + len], kind), Ok(p.expected()));
        }
        let first_disc = pkts.iter().find(|p| p.code == 50).map(|p| p.reason);
        prop_assert_eq!(stacked_depth_disconnect_reason(&frame, kind, MAX_PACKETS), first_disc);
    }

    #[test]
    fn prop_depth_truncated_at_every_byte(
        (kind, pkts) in kinds().prop_flat_map(|kind| {
            (Just(kind), prop::collection::vec(gen_depth_pkt(kind, vec![41, 51, 50], small_rows()), 1..4))
        }),
    ) {
        let (frame, bounds) = depth_stack(&pkts);
        let ep = endpoint_for(kind);
        for cut in 0..=frame.len() {
            let sub = &frame[..cut];
            let (split, stop, _) = prod_split(sub, kind);
            let complete = bounds.iter().take_while(|&&(o, l)| o + l <= cut).count();
            prop_assert_eq!(&split[..], &bounds[..complete], "cut {}", cut);
            let on_boundary = complete == pkts.len() || bounds[complete].0 == cut;
            if on_boundary {
                prop_assert_eq!(stop, DepthSplitStop::Complete, "cut {}", cut);
            } else {
                let (start, size) = bounds[complete];
                let actual = cut - start;
                let expected_stop = if actual < DEPTH_HEADER_LEN {
                    DepthSplitStop::TruncatedHeader { remaining: actual }
                } else {
                    DepthSplitStop::TruncatedBody { expected: size, actual }
                };
                prop_assert_eq!(stop, expected_stop, "cut {}", cut);
                // The straddling remainder is refused, never decoded.
                let expected_len = if actual < DEPTH_HEADER_LEN { DEPTH_HEADER_LEN } else { size };
                prop_assert_eq!(
                    depth_dispatch(&frame[start..cut], kind),
                    Err(RefErr::Insufficient { expected: expected_len, actual }),
                    "cut {}", cut
                );
            }
            let model_disc = if on_boundary {
                pkts[..complete].iter().find(|p| p.code == 50).map(|p| p.reason)
            } else {
                None
            };
            prop_assert_eq!(
                stacked_depth_disconnect_reason(sub, kind, MAX_PACKETS),
                model_disc,
                "cut {}", cut
            );
            let model_class = model_disc.map_or(FrameClass::Data, |r| {
                FrameClass::Disconnect(DisconnectCode::from_u16(r))
            });
            prop_assert_eq!(classify_frame(ep, sub), model_class, "cut {}", cut);
        }
    }

    #[test]
    fn prop_depth_disconnect_stacked_behind_books_is_classified(
        (kind, books, disc, after) in kinds().prop_flat_map(|kind| {
            (
                Just(kind),
                prop::collection::vec(gen_depth_pkt(kind, vec![41, 51], small_rows()), 0..5),
                gen_depth_pkt(kind, vec![50], small_rows()),
                prop::collection::vec(gen_depth_pkt(kind, vec![41, 51, 50], small_rows()), 0..3),
            )
        }),
    ) {
        let mut all = books.clone();
        all.push(disc.clone());
        all.extend(after.iter().cloned());
        let (frame, _) = depth_stack(&all);
        let n = all.len() as u32;
        prop_assert_eq!(
            classify_frame(endpoint_for(kind), &frame),
            FrameClass::Disconnect(DisconnectCode::from_u16(disc.reason))
        );
        prop_assert_eq!(stacked_depth_disconnect_reason(&frame, kind, n), Some(disc.reason));
        prop_assert_eq!(stacked_depth_disconnect_reason(&frame, kind, n - 1), None);
        // The same bytes on every other endpoint match the oracle (depth bytes
        // on the main feed must not invent a disconnect the oracle lacks).
        for ep in DhanEndpointType::ALL {
            prop_assert_eq!(classify_frame(ep, &frame), oracle_classify(ep, &frame));
        }
    }

    #[test]
    fn prop_depth_bit_flip_is_localised(
        (kind, pkt) in kinds().prop_flat_map(|kind| {
            (Just(kind), gen_depth_pkt(kind, vec![41, 51, 50], tiny_rows()))
        }),
    ) {
        let mut p = pkt;
        p.stamp = None;
        let bytes = p.encode();
        let orig = depth_dispatch(&bytes, kind).expect("valid depth packet");
        prop_assert_eq!(&orig, &p.expected());
        for byte in 0..bytes.len() {
            for bit in 0..8 {
                let mut flipped = bytes.clone();
                flipped[byte] ^= 1 << bit;
                let got = depth_dispatch(&flipped, kind);
                let row_count_byte = (8..12).contains(&byte)
                    && kind == DepthFeedKind::TwoHundred
                    && p.code != 50;
                if byte == 2 || row_count_byte {
                    // Code or depth-200 row count: the packet's shape changes;
                    // the result must be exactly what the reference reads.
                    prop_assert_eq!(got, ref_depth_decode(&flipped, kind), "byte {} bit {}", byte, bit);
                    continue;
                }
                let got = got.expect("non-shape flip keeps the packet decodable");
                let owner: (&'static str, u32) = match byte {
                    0 | 1 => ("msg_len", 0),
                    3 => ("segment", 0),
                    4..=7 => ("security_id", 0),
                    8..=11 => ("seq", 0),
                    _ if p.code == 50 => ("reason", 0),
                    _ => {
                        let rel = byte - 12;
                        let name = match rel % 16 {
                            0..=7 => "price",
                            8..=11 => "qty",
                            _ => "orders",
                        };
                        (name, (rel / 16) as u32)
                    }
                };
                prop_assert_eq!(
                    diff_keys(&orig, &got),
                    vec![owner],
                    "kind {:?} code {} byte {} bit {}", kind, p.code, byte, bit
                );
            }
        }
    }
}

// ===========================================================================
// Arbitrary input: everything equals the reference, nothing panics
// ===========================================================================

#[derive(Debug, Clone)]
enum Chunk {
    Main(GenPkt),
    Depth20(DGen),
    Depth200(DGen),
    Raw(Vec<u8>),
}

fn gen_chunk() -> impl Strategy<Value = Chunk> {
    prop_oneof![
        3 => gen_pkt().prop_map(Chunk::Main),
        2 => gen_depth_pkt(DepthFeedKind::Twenty, vec![41, 51, 50], tiny_rows()).prop_map(Chunk::Depth20),
        2 => gen_depth_pkt(DepthFeedKind::TwoHundred, vec![41, 51, 50], tiny_rows()).prop_map(Chunk::Depth200),
        2 => prop::collection::vec(any::<u8>(), 0..48).prop_map(Chunk::Raw),
    ]
}

fn assemble(chunks: &[Chunk], flips: &[(usize, u8)]) -> Vec<u8> {
    let mut frame = Vec::new();
    for c in chunks {
        match c {
            Chunk::Main(p) => frame.extend_from_slice(&p.encode()),
            Chunk::Depth20(d) | Chunk::Depth200(d) => frame.extend_from_slice(&d.encode()),
            Chunk::Raw(r) => frame.extend_from_slice(r),
        }
    }
    if !frame.is_empty() {
        for &(pos, bit) in flips {
            let i = pos % frame.len();
            frame[i] ^= 1 << (bit % 8);
        }
    }
    frame
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1024))]

    #[test]
    fn prop_arbitrary_bytes_match_the_reference_everywhere(
        chunks in prop::collection::vec(gen_chunk(), 0..8),
        flips in prop::collection::vec((any::<usize>(), any::<u8>()), 0..4),
        recv in any::<i64>(),
    ) {
        let frame = assemble(&chunks, &flips);

        // dispatch_frame on every suffix (every place a walk could land).
        for start in 0..=frame.len() {
            let got = dispatch_frame(&frame[start..], recv).map(|f| canon_of(&f)).map_err(|e| err_canon(&e));
            prop_assert_eq!(got, ref_decode(&frame[start..], recv), "suffix at {}", start);
            // The length table and the dispatcher agree on what is known.
            let rest = &frame[start..];
            if let Some(&code) = rest.first() {
                prop_assert_eq!(main_feed_packet_len(rest), ref_size(code));
            } else {
                prop_assert_eq!(main_feed_packet_len(rest), None);
            }
        }

        prop_assert_eq!(stacked_disconnect_reason(&frame, MAX_PACKETS), ref_main_stacked(&frame));
        for ep in DhanEndpointType::ALL {
            prop_assert_eq!(classify_frame(ep, &frame), oracle_classify(ep, &frame), "{:?}", ep);
        }

        for kind in [DepthFeedKind::Twenty, DepthFeedKind::TwoHundred] {
            let (split, stop, mismatches) = prod_split(&frame, kind);
            let (r_split, r_stop, r_mismatches) = ref_depth_split(&frame, kind);
            prop_assert_eq!(&split, &r_split, "{:?}", kind);
            prop_assert_eq!(stop, r_stop, "{:?}", kind);
            prop_assert_eq!(mismatches, r_mismatches, "{:?}", kind);
            for &(off, len) in &split {
                prop_assert_eq!(
                    depth_dispatch(&frame[off..off + len], kind),
                    ref_depth_decode(&frame[off..off + len], kind)
                );
            }
            prop_assert_eq!(depth_dispatch(&frame, kind), ref_depth_decode(&frame, kind));
            prop_assert_eq!(
                stacked_depth_disconnect_reason(&frame, kind, MAX_PACKETS),
                ref_depth_stacked(&frame, kind)
            );
        }
    }

    #[test]
    fn prop_raw_bytes_match_the_reference(
        frame in prop::collection::vec(any::<u8>(), 0..600),
        steer in prop::option::of(prop::sample::select(vec![1u8, 2, 3, 4, 5, 6, 7, 8, 41, 50, 51])),
        recv in any::<i64>(),
    ) {
        let mut frame = frame;
        // Steer byte 0 (main-feed code) and byte 2 (depth code) so arbitrary
        // input reaches the per-code arms instead of stalling on "unknown".
        if let Some(code) = steer {
            if let Some(b) = frame.get_mut(0) { *b = code; }
            if let Some(b) = frame.get_mut(2) { *b = code; }
        }
        let got = dispatch_frame(&frame, recv).map(|f| canon_of(&f)).map_err(|e| err_canon(&e));
        prop_assert_eq!(got, ref_decode(&frame, recv));
        prop_assert_eq!(stacked_disconnect_reason(&frame, MAX_PACKETS), ref_main_stacked(&frame));
        for ep in DhanEndpointType::ALL {
            prop_assert_eq!(classify_frame(ep, &frame), oracle_classify(ep, &frame), "{:?}", ep);
        }
        for kind in [DepthFeedKind::Twenty, DepthFeedKind::TwoHundred] {
            prop_assert_eq!(prod_split(&frame, kind), ref_depth_split(&frame, kind));
            prop_assert_eq!(depth_dispatch(&frame, kind), ref_depth_decode(&frame, kind));
        }
    }
}

// ===========================================================================
// Pinned extremes (deterministic, so a regression names itself)
// ===========================================================================

#[test]
fn every_byte_value_as_a_main_feed_code_is_classified_exactly() {
    for code in 0..=u8::MAX {
        for len in [0usize, 1, 7, 8, 9, 10, 11, 12, 16, 50, 161, 162, 163] {
            let mut frame = vec![0u8; len];
            if let Some(b) = frame.first_mut() {
                *b = code;
            }
            let got = dispatch_frame(&frame, 0)
                .map(|f| canon_of(&f))
                .map_err(|e| err_canon(&e));
            assert_eq!(got, ref_decode(&frame, 0), "code {code} len {len}");
            for ep in DhanEndpointType::ALL {
                assert_eq!(
                    classify_frame(ep, &frame),
                    oracle_classify(ep, &frame),
                    "code {code} len {len} {ep:?}"
                );
            }
        }
    }
}

#[test]
fn depth_200_row_count_edges_are_exact() {
    for rows in [0u32, 1, 199, 200, 201, 65_535, u32::MAX] {
        let mut frame = vec![0u8; 12 + 16 * 200 + 16];
        frame[2] = 41;
        write_le(&mut frame, 8, 4, u64::from(rows));
        let kind = DepthFeedKind::TwoHundred;
        assert_eq!(
            depth_dispatch(&frame, kind),
            ref_depth_decode(&frame, kind),
            "rows {rows}"
        );
        assert_eq!(
            prod_split(&frame, kind),
            ref_depth_split(&frame, kind),
            "rows {rows}"
        );
        if rows > 200 {
            assert_eq!(
                depth_dispatch(&frame, kind),
                Err(RefErr::RowCount {
                    actual: rows,
                    max: 200
                })
            );
        }
    }
}
