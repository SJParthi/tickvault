//! Native Rust decoder for the Groww live-feed protobuf payload (operator
//! 2026-06-19 "implement everything"; §32 native-Rust path).
//!
//! Groww's live feed is NATS-over-WebSocket; each `MSG` frame's binary body is a
//! protobuf-encoded `StocksSocketResponseProtoDto`. This module decodes that body
//! into plain Rust structs WITHOUT any protobuf crate dependency — the schema was
//! extracted verbatim from the official `growwapi-1.5.0` SDK descriptor
//! (`groww/proto/stocks_socket_response_pb2.py`), see `docs/groww-ref/`.
//!
//! ## Verified wire schema (from the SDK descriptor — NOT guessed)
//!
//! `StocksSocketResponseProtoDto` (the MSG body):
//! | # | field | wire |
//! |---|-------|------|
//! | 1 | symbol | string (len-delimited) |
//! | 2 | segment | enum (varint) |
//! | 3 | exchange | enum (varint) |
//! | 4 | stockLivePrice | message → [`GrowwLivePrice`] |
//! | 5 | stocksMarketDepth | message (not decoded here) |
//! | 6 | stocksLiveIndices | message → [`GrowwIndexValue`] |
//!
//! `StocksLivePriceProto` — **every field is a `double` (protobuf wire type 1,
//! fixed64 IEEE-754 little-endian)**, including `tsInMillis`/`volume`/`ltp`:
//! 1 tsInMillis · 2 open · 3 high · 4 low · 5 close · 6 volume · 7 value ·
//! 8 bidQty · 9 offerQty · 10 avgPrice · 11 highPriceRange · 12 lowPriceRange ·
//! 13 ltp · 14 openInterest · 15 lowTradeRange · 16 highTradeRange.
//!
//! `StocksLiveIndicesProto`: 1 tsInMillis (double) · 2 value (double).
//!
//! ## Identity is NOT in the body
//!
//! Per the SDK, the instrument identity (exchange/segment/token) comes from the
//! NATS **subscription subject** (`/ld/eq/nse/price.<token>`), not the tick body.
//! So this decoder yields ONLY the price payload; the connector pairs it with the
//! subject's token. The body's `symbol`/`segment`/`exchange` are skipped.
//!
//! ## Guarantees (operator O(1) / worst-case demand)
//!
//! - **Zero allocation, O(field-count):** the decoder walks a `&[u8]` with a
//!   cursor; nested messages are sub-slices (no copy). The output structs are
//!   `Copy` stack values. No `Vec`/`String`/`Box`.
//! - **No panic on any input:** every read is bounds-checked and returns a typed
//!   [`ProtoDecodeError`] (a `Copy` enum — no heap even on the error path).
//!   Truncated/garbage/over-long-varint/unknown-field inputs all return `Err` or
//!   skip cleanly; fuzz-style adversarial bytes can never panic.
//! - **Forward-compatible:** unknown field numbers + unexpected wire types are
//!   skipped, so a future Groww schema addition does not break decoding.

/// Field numbers in `StocksSocketResponseProtoDto` (the MSG body).
const FIELD_STOCK_LIVE_PRICE: u64 = 4;
const FIELD_STOCKS_LIVE_INDICES: u64 = 6;

/// Field numbers in `StocksLivePriceProto` (all wire-type-1 doubles).
const LP_TS_IN_MILLIS: u64 = 1;
const LP_OPEN: u64 = 2;
const LP_HIGH: u64 = 3;
const LP_LOW: u64 = 4;
const LP_CLOSE: u64 = 5;
const LP_VOLUME: u64 = 6;
const LP_VALUE: u64 = 7;
const LP_BID_QTY: u64 = 8;
const LP_OFFER_QTY: u64 = 9;
const LP_AVG_PRICE: u64 = 10;
const LP_HIGH_PRICE_RANGE: u64 = 11;
const LP_LOW_PRICE_RANGE: u64 = 12;
const LP_LTP: u64 = 13;
const LP_OPEN_INTEREST: u64 = 14;
const LP_LOW_TRADE_RANGE: u64 = 15;
const LP_HIGH_TRADE_RANGE: u64 = 16;

/// Field numbers in `StocksLiveIndicesProto`.
const IDX_TS_IN_MILLIS: u64 = 1;
const IDX_VALUE: u64 = 2;

/// Protobuf wire types we handle.
const WIRE_VARINT: u8 = 0;
const WIRE_FIXED64: u8 = 1;
const WIRE_LEN_DELIM: u8 = 2;
const WIRE_FIXED32: u8 = 5;

/// A decode failure. `Copy` (no heap) so the error path is also zero-alloc.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProtoDecodeError {
    /// Ran off the end of the buffer mid-field.
    Truncated,
    /// A varint exceeded 10 bytes (malformed / not a valid u64 LEB128).
    VarintOverflow,
    /// A wire type we do not recognise (3/4 group-start/end, or 6/7).
    UnknownWireType(u8),
}

impl core::fmt::Display for ProtoDecodeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Truncated => write!(f, "groww proto decode: truncated input"),
            Self::VarintOverflow => write!(f, "groww proto decode: varint overflow (>10 bytes)"),
            Self::UnknownWireType(w) => {
                write!(f, "groww proto decode: unknown wire type {w}")
            }
        }
    }
}

impl std::error::Error for ProtoDecodeError {}

/// Decoded `StocksLivePriceProto` — the LTP feed payload. All fields `f64` exactly
/// as on the wire (Groww emits every field as a `double`). Absent fields default
/// to `0.0` (proto3 semantics). The millisecond timestamp is the Groww advantage
/// over Dhan's whole-second LTT.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct GrowwLivePrice {
    pub ts_in_millis: f64,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: f64,
    pub value: f64,
    pub bid_qty: f64,
    pub offer_qty: f64,
    pub avg_price: f64,
    pub high_price_range: f64,
    pub low_price_range: f64,
    pub ltp: f64,
    pub open_interest: f64,
    pub low_trade_range: f64,
    pub high_trade_range: f64,
}

/// Decoded `StocksLiveIndicesProto` — the index-value feed payload.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct GrowwIndexValue {
    pub ts_in_millis: f64,
    pub value: f64,
}

/// The decoded top-level message — exactly one payload kind is present per frame
/// (Groww sends a price frame XOR an index frame XOR depth, which we ignore).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum GrowwSocketMessage {
    /// `stockLivePrice` (field 4) was present.
    LivePrice(GrowwLivePrice),
    /// `stocksLiveIndices` (field 6) was present.
    Index(GrowwIndexValue),
    /// Neither a price nor an index payload (e.g. depth-only, or a market-info
    /// frame). The caller skips it — never an error.
    Other,
}

/// Zero-copy protobuf cursor over a byte slice. No allocation; bounds-checked.
struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    #[inline]
    const fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    #[inline]
    const fn at_end(&self) -> bool {
        self.pos >= self.buf.len()
    }

    /// LEB128 varint, max 10 bytes (a u64). Returns `VarintOverflow` past 10.
    #[inline]
    fn read_varint(&mut self) -> Result<u64, ProtoDecodeError> {
        let mut result: u64 = 0;
        let mut shift: u32 = 0;
        loop {
            if shift >= 64 {
                return Err(ProtoDecodeError::VarintOverflow);
            }
            let byte = *self.buf.get(self.pos).ok_or(ProtoDecodeError::Truncated)?;
            self.pos += 1;
            result |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Ok(result);
            }
            shift += 7;
        }
    }

    /// Tag = `(field_number << 3) | wire_type`.
    #[inline]
    fn read_tag(&mut self) -> Result<(u64, u8), ProtoDecodeError> {
        let tag = self.read_varint()?;
        Ok((tag >> 3, (tag & 0x07) as u8))
    }

    /// 8 little-endian bytes → `f64`.
    #[inline]
    fn read_f64(&mut self) -> Result<f64, ProtoDecodeError> {
        let end = self.pos.checked_add(8).ok_or(ProtoDecodeError::Truncated)?;
        let bytes = self
            .buf
            .get(self.pos..end)
            .ok_or(ProtoDecodeError::Truncated)?;
        let arr: [u8; 8] = bytes.try_into().map_err(|_| ProtoDecodeError::Truncated)?;
        self.pos = end;
        Ok(f64::from_le_bytes(arr))
    }

    /// A length-delimited field → its inner byte slice (no copy).
    #[inline]
    fn read_len_delimited(&mut self) -> Result<&'a [u8], ProtoDecodeError> {
        let len = self.read_varint()? as usize;
        let end = self
            .pos
            .checked_add(len)
            .ok_or(ProtoDecodeError::Truncated)?;
        let slice = self
            .buf
            .get(self.pos..end)
            .ok_or(ProtoDecodeError::Truncated)?;
        self.pos = end;
        Ok(slice)
    }

    /// Skip a field of the given wire type (forward-compat for unknown fields).
    #[inline]
    fn skip(&mut self, wire: u8) -> Result<(), ProtoDecodeError> {
        match wire {
            WIRE_VARINT => {
                self.read_varint()?;
                Ok(())
            }
            WIRE_FIXED64 => {
                let end = self.pos.checked_add(8).ok_or(ProtoDecodeError::Truncated)?;
                if end > self.buf.len() {
                    return Err(ProtoDecodeError::Truncated);
                }
                self.pos = end;
                Ok(())
            }
            WIRE_LEN_DELIM => {
                self.read_len_delimited()?;
                Ok(())
            }
            WIRE_FIXED32 => {
                let end = self.pos.checked_add(4).ok_or(ProtoDecodeError::Truncated)?;
                if end > self.buf.len() {
                    return Err(ProtoDecodeError::Truncated);
                }
                self.pos = end;
                Ok(())
            }
            other => Err(ProtoDecodeError::UnknownWireType(other)),
        }
    }
}

/// Decode a `StocksLivePriceProto` body (every field a fixed64 double). Unknown
/// fields / wire types are skipped (forward-compatible). Never panics.
pub fn decode_live_price(buf: &[u8]) -> Result<GrowwLivePrice, ProtoDecodeError> {
    let mut r = Reader::new(buf);
    let mut lp = GrowwLivePrice::default();
    while !r.at_end() {
        let (field, wire) = r.read_tag()?;
        if wire == WIRE_FIXED64 {
            let v = r.read_f64()?;
            match field {
                LP_TS_IN_MILLIS => lp.ts_in_millis = v,
                LP_OPEN => lp.open = v,
                LP_HIGH => lp.high = v,
                LP_LOW => lp.low = v,
                LP_CLOSE => lp.close = v,
                LP_VOLUME => lp.volume = v,
                LP_VALUE => lp.value = v,
                LP_BID_QTY => lp.bid_qty = v,
                LP_OFFER_QTY => lp.offer_qty = v,
                LP_AVG_PRICE => lp.avg_price = v,
                LP_HIGH_PRICE_RANGE => lp.high_price_range = v,
                LP_LOW_PRICE_RANGE => lp.low_price_range = v,
                LP_LTP => lp.ltp = v,
                LP_OPEN_INTEREST => lp.open_interest = v,
                LP_LOW_TRADE_RANGE => lp.low_trade_range = v,
                LP_HIGH_TRADE_RANGE => lp.high_trade_range = v,
                _ => {} // unknown field, already consumed the 8 bytes
            }
        } else {
            r.skip(wire)?;
        }
    }
    Ok(lp)
}

/// Decode a `StocksLiveIndicesProto` body.
pub fn decode_index_value(buf: &[u8]) -> Result<GrowwIndexValue, ProtoDecodeError> {
    let mut r = Reader::new(buf);
    let mut idx = GrowwIndexValue::default();
    while !r.at_end() {
        let (field, wire) = r.read_tag()?;
        if wire == WIRE_FIXED64 {
            let v = r.read_f64()?;
            match field {
                IDX_TS_IN_MILLIS => idx.ts_in_millis = v,
                IDX_VALUE => idx.value = v,
                _ => {}
            }
        } else {
            r.skip(wire)?;
        }
    }
    Ok(idx)
}

/// Decode a `StocksSocketResponseProtoDto` MSG body into the present payload.
///
/// Walks the top-level fields; the first `stockLivePrice` (field 4) wins as a
/// [`GrowwSocketMessage::LivePrice`], else the first `stocksLiveIndices` (field 6)
/// as [`GrowwSocketMessage::Index`], else [`GrowwSocketMessage::Other`]. The
/// `symbol`/`segment`/`exchange`/`stocksMarketDepth` fields are skipped (identity
/// comes from the NATS subject). Never panics on any input.
pub fn decode_socket_response(buf: &[u8]) -> Result<GrowwSocketMessage, ProtoDecodeError> {
    let mut r = Reader::new(buf);
    while !r.at_end() {
        let (field, wire) = r.read_tag()?;
        if wire == WIRE_LEN_DELIM && field == FIELD_STOCK_LIVE_PRICE {
            let inner = r.read_len_delimited()?;
            return Ok(GrowwSocketMessage::LivePrice(decode_live_price(inner)?));
        } else if wire == WIRE_LEN_DELIM && field == FIELD_STOCKS_LIVE_INDICES {
            let inner = r.read_len_delimited()?;
            return Ok(GrowwSocketMessage::Index(decode_index_value(inner)?));
        }
        r.skip(wire)?;
    }
    Ok(GrowwSocketMessage::Other)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Encode `(field, f64)` as a protobuf wire-type-1 (fixed64) entry.
    fn enc_double(field: u64, v: f64) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&enc_varint((field << 3) | u64::from(WIRE_FIXED64)));
        out.extend_from_slice(&v.to_le_bytes());
        out
    }

    fn enc_varint(mut v: u64) -> Vec<u8> {
        let mut out = Vec::new();
        loop {
            let mut byte = (v & 0x7f) as u8;
            v >>= 7;
            if v != 0 {
                byte |= 0x80;
            }
            out.push(byte);
            if v == 0 {
                break;
            }
        }
        out
    }

    /// Wrap an inner message as a length-delimited field.
    fn enc_message(field: u64, inner: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&enc_varint((field << 3) | u64::from(WIRE_LEN_DELIM)));
        out.extend_from_slice(&enc_varint(inner.len() as u64));
        out.extend_from_slice(inner);
        out
    }

    #[test]
    fn test_decode_live_price_core_fields() {
        let mut body = Vec::new();
        body.extend_from_slice(&enc_double(LP_TS_IN_MILLIS, 1_780_000_020_123.0));
        body.extend_from_slice(&enc_double(LP_LTP, 2_847.55));
        body.extend_from_slice(&enc_double(LP_VOLUME, 123_456.0));
        body.extend_from_slice(&enc_double(LP_OPEN, 2_840.0));
        body.extend_from_slice(&enc_double(LP_HIGH, 2_850.5));
        body.extend_from_slice(&enc_double(LP_LOW, 2_838.0));
        body.extend_from_slice(&enc_double(LP_CLOSE, 2_847.55));
        body.extend_from_slice(&enc_double(LP_OPEN_INTEREST, 9_900.0));
        let lp = decode_live_price(&body).expect("decode");
        assert_eq!(lp.ts_in_millis, 1_780_000_020_123.0);
        assert_eq!(lp.ltp, 2_847.55);
        assert_eq!(lp.volume, 123_456.0);
        assert_eq!(lp.open, 2_840.0);
        assert_eq!(lp.high, 2_850.5);
        assert_eq!(lp.low, 2_838.0);
        assert_eq!(lp.close, 2_847.55);
        assert_eq!(lp.open_interest, 9_900.0);
    }

    #[test]
    fn test_absent_fields_default_to_zero() {
        let body = enc_double(LP_LTP, 100.0); // only ltp present
        let lp = decode_live_price(&body).expect("decode");
        assert_eq!(lp.ltp, 100.0);
        assert_eq!(lp.volume, 0.0);
        assert_eq!(lp.ts_in_millis, 0.0);
    }

    #[test]
    fn test_decode_index_value() {
        let mut body = Vec::new();
        body.extend_from_slice(&enc_double(IDX_TS_IN_MILLIS, 1_780_000_000_000.0));
        body.extend_from_slice(&enc_double(IDX_VALUE, 23_146.45));
        let idx = decode_index_value(&body).expect("decode");
        assert_eq!(idx.ts_in_millis, 1_780_000_000_000.0);
        assert_eq!(idx.value, 23_146.45);
    }

    #[test]
    fn test_decode_socket_response_routes_to_live_price() {
        let mut inner = Vec::new();
        inner.extend_from_slice(&enc_double(LP_LTP, 555.5));
        let frame = enc_message(FIELD_STOCK_LIVE_PRICE, &inner);
        match decode_socket_response(&frame).expect("decode") {
            GrowwSocketMessage::LivePrice(lp) => assert_eq!(lp.ltp, 555.5),
            other => panic!("expected LivePrice, got {other:?}"),
        }
    }

    #[test]
    fn test_socket_response_routes_to_index() {
        let mut inner = Vec::new();
        inner.extend_from_slice(&enc_double(IDX_VALUE, 47_000.0));
        let frame = enc_message(FIELD_STOCKS_LIVE_INDICES, &inner);
        match decode_socket_response(&frame).expect("decode") {
            GrowwSocketMessage::Index(idx) => assert_eq!(idx.value, 47_000.0),
            other => panic!("expected Index, got {other:?}"),
        }
    }

    #[test]
    fn test_socket_response_skips_symbol_and_depth_fields() {
        // field 1 (symbol, string) + field 5 (depth, message) must be skipped,
        // then field 4 (live price) decoded.
        let mut frame = Vec::new();
        // symbol = "RELIANCE" (field 1, len-delim)
        frame.extend_from_slice(&enc_varint((1 << 3) | u64::from(WIRE_LEN_DELIM)));
        frame.extend_from_slice(&enc_varint(8));
        frame.extend_from_slice(b"RELIANCE");
        // segment enum (field 2, varint)
        frame.extend_from_slice(&enc_varint((2 << 3) | u64::from(WIRE_VARINT)));
        frame.extend_from_slice(&enc_varint(1));
        // depth (field 5, message) with arbitrary inner
        frame.extend_from_slice(&enc_message(5, &enc_double(1, 9.0)));
        // the live price (field 4)
        frame.extend_from_slice(&enc_message(
            FIELD_STOCK_LIVE_PRICE,
            &enc_double(LP_LTP, 42.0),
        ));
        match decode_socket_response(&frame).expect("decode") {
            GrowwSocketMessage::LivePrice(lp) => assert_eq!(lp.ltp, 42.0),
            other => panic!("expected LivePrice, got {other:?}"),
        }
    }

    #[test]
    fn test_empty_input_is_other_not_error() {
        assert_eq!(
            decode_socket_response(&[]).expect("empty ok"),
            GrowwSocketMessage::Other
        );
        assert_eq!(
            decode_live_price(&[]).expect("empty ok"),
            GrowwLivePrice::default()
        );
    }

    #[test]
    fn test_truncated_fixed64_errors_not_panics() {
        // tag for ltp then only 3 of 8 bytes
        let mut body = enc_varint((LP_LTP << 3) | u64::from(WIRE_FIXED64));
        body.extend_from_slice(&[1, 2, 3]);
        assert_eq!(decode_live_price(&body), Err(ProtoDecodeError::Truncated));
    }

    #[test]
    fn test_truncated_len_delim_errors() {
        // field 4 message claims length 200 but no bytes follow
        let mut frame = enc_varint((FIELD_STOCK_LIVE_PRICE << 3) | u64::from(WIRE_LEN_DELIM));
        frame.extend_from_slice(&enc_varint(200));
        assert_eq!(
            decode_socket_response(&frame),
            Err(ProtoDecodeError::Truncated)
        );
    }

    #[test]
    fn test_unknown_wire_type_errors_cleanly() {
        // wire type 3 (group start) is not handled → UnknownWireType(3)
        let body = enc_varint((1 << 3) | 3);
        assert_eq!(
            decode_live_price(&body),
            Err(ProtoDecodeError::UnknownWireType(3))
        );
    }

    #[test]
    fn test_varint_overflow_errors_not_panics() {
        // 11 continuation bytes → overflow
        let body = vec![0xff_u8; 11];
        assert_eq!(
            decode_live_price(&body),
            Err(ProtoDecodeError::VarintOverflow)
        );
    }

    #[test]
    fn test_unknown_field_is_skipped_forward_compat() {
        // a future field 99 (fixed64) before the known ltp must be skipped, not fail.
        let mut body = enc_double(99, 7.0);
        body.extend_from_slice(&enc_double(LP_LTP, 11.0));
        let lp = decode_live_price(&body).expect("decode");
        assert_eq!(lp.ltp, 11.0);
    }

    #[test]
    fn test_unknown_wire_types_6_and_7_error_cleanly() {
        // wire types 6 and 7 are not valid protobuf — must error, never panic.
        for wire in [6u64, 7u64] {
            let body = enc_varint((1 << 3) | wire);
            assert_eq!(
                decode_live_price(&body),
                Err(ProtoDecodeError::UnknownWireType(wire as u8))
            );
        }
    }

    #[test]
    fn test_len_delimited_huge_length_is_truncated_not_oom() {
        // A length-delimited field claiming a massive length must return
        // Truncated (bounds-checked), never attempt a huge read/alloc or overflow.
        let mut frame = enc_varint((FIELD_STOCK_LIVE_PRICE << 3) | u64::from(WIRE_LEN_DELIM));
        frame.extend_from_slice(&enc_varint(u64::MAX)); // absurd length
        assert_eq!(
            decode_socket_response(&frame),
            Err(ProtoDecodeError::Truncated)
        );
    }

    #[test]
    fn test_truncated_len_varint_errors() {
        // tag for a len-delimited field, then a lone 0x80 (continuation bit set,
        // no terminating byte) — the length varint runs off the end.
        let mut frame = enc_varint((FIELD_STOCK_LIVE_PRICE << 3) | u64::from(WIRE_LEN_DELIM));
        frame.push(0x80);
        assert_eq!(
            decode_socket_response(&frame),
            Err(ProtoDecodeError::Truncated)
        );
    }

    #[test]
    fn test_buffer_ends_immediately_after_tag_is_truncated() {
        // a fixed64 tag with zero value bytes following.
        let body = enc_varint((LP_LTP << 3) | u64::from(WIRE_FIXED64));
        assert_eq!(decode_live_price(&body), Err(ProtoDecodeError::Truncated));
    }

    #[test]
    fn test_decode_error_display_is_stable() {
        assert!(
            ProtoDecodeError::Truncated
                .to_string()
                .contains("truncated")
        );
        assert!(
            ProtoDecodeError::UnknownWireType(3)
                .to_string()
                .contains("wire type 3")
        );
    }
}
