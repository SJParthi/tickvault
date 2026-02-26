//! Quote packet parser (50 bytes).
//!
//! Format: `<BHBIfHIfIIIffff>` — Header(8) + LTP + LTQ + LTT + ATP + Vol + TSQ + TBQ + OHLC.
//! CRITICAL: OHLC starts at offset 34 (diverges from Full packet at this point).

use dhan_live_trader_common::constants::{
    QUOTE_OFFSET_ATP, QUOTE_OFFSET_CLOSE, QUOTE_OFFSET_HIGH, QUOTE_OFFSET_LOW, QUOTE_OFFSET_LTP,
    QUOTE_OFFSET_LTQ, QUOTE_OFFSET_LTT, QUOTE_OFFSET_OPEN, QUOTE_OFFSET_TOTAL_BUY_QTY,
    QUOTE_OFFSET_TOTAL_SELL_QTY, QUOTE_OFFSET_VOLUME, QUOTE_PACKET_SIZE,
};
use dhan_live_trader_common::tick_types::ParsedTick;

use super::types::{PacketHeader, ParseError};

/// Helper: reads a little-endian f32 from a byte slice at the given offset.
#[inline(always)]
fn read_f32_le(raw: &[u8], offset: usize) -> f32 {
    f32::from_le_bytes([
        raw[offset],
        raw[offset + 1],
        raw[offset + 2],
        raw[offset + 3],
    ])
}

/// Helper: reads a little-endian u32 from a byte slice at the given offset.
#[inline(always)]
fn read_u32_le(raw: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        raw[offset],
        raw[offset + 1],
        raw[offset + 2],
        raw[offset + 3],
    ])
}

/// Parses a Quote packet into a `ParsedTick`.
///
/// All fields except OI-related ones are populated. OI fields stay at 0.
///
/// # Performance
/// O(1) — eleven `from_le_bytes` reads.
pub fn parse_quote_packet(
    raw: &[u8],
    header: &PacketHeader,
    received_at_nanos: i64,
) -> Result<ParsedTick, ParseError> {
    if raw.len() < QUOTE_PACKET_SIZE {
        return Err(ParseError::InsufficientBytes {
            expected: QUOTE_PACKET_SIZE,
            actual: raw.len(),
        });
    }

    let ltp = read_f32_le(raw, QUOTE_OFFSET_LTP);
    let ltq = u16::from_le_bytes([raw[QUOTE_OFFSET_LTQ], raw[QUOTE_OFFSET_LTQ + 1]]);
    let ltt = read_u32_le(raw, QUOTE_OFFSET_LTT);
    let atp = read_f32_le(raw, QUOTE_OFFSET_ATP);
    let volume = read_u32_le(raw, QUOTE_OFFSET_VOLUME);
    let total_sell_qty = read_u32_le(raw, QUOTE_OFFSET_TOTAL_SELL_QTY);
    let total_buy_qty = read_u32_le(raw, QUOTE_OFFSET_TOTAL_BUY_QTY);
    let day_open = read_f32_le(raw, QUOTE_OFFSET_OPEN);
    let day_close = read_f32_le(raw, QUOTE_OFFSET_CLOSE);
    let day_high = read_f32_le(raw, QUOTE_OFFSET_HIGH);
    let day_low = read_f32_le(raw, QUOTE_OFFSET_LOW);

    Ok(ParsedTick {
        security_id: header.security_id,
        exchange_segment_code: header.exchange_segment_code,
        last_traded_price: ltp,
        last_trade_quantity: ltq,
        exchange_timestamp: ltt,
        received_at_nanos,
        average_traded_price: atp,
        volume,
        total_sell_quantity: total_sell_qty,
        total_buy_quantity: total_buy_qty,
        day_open,
        day_close,
        day_high,
        day_low,
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use dhan_live_trader_common::constants::EXCHANGE_SEGMENT_NSE_FNO;

    /// Builds a complete 50-byte Quote packet with all fields.
    fn make_quote_packet(
        segment: u8,
        security_id: u32,
        ltp: f32,
        ltq: u16,
        ltt: u32,
        atp: f32,
        volume: u32,
        total_sell: u32,
        total_buy: u32,
        open: f32,
        close: f32,
        high: f32,
        low: f32,
    ) -> (Vec<u8>, PacketHeader) {
        let mut buf = vec![0u8; QUOTE_PACKET_SIZE];
        // header
        buf[0] = 4; // RESPONSE_CODE_QUOTE
        buf[1..3].copy_from_slice(&(QUOTE_PACKET_SIZE as u16).to_le_bytes());
        buf[3] = segment;
        buf[4..8].copy_from_slice(&security_id.to_le_bytes());
        // body
        buf[8..12].copy_from_slice(&ltp.to_le_bytes());
        buf[12..14].copy_from_slice(&ltq.to_le_bytes());
        buf[14..18].copy_from_slice(&ltt.to_le_bytes());
        buf[18..22].copy_from_slice(&atp.to_le_bytes());
        buf[22..26].copy_from_slice(&volume.to_le_bytes());
        buf[26..30].copy_from_slice(&total_sell.to_le_bytes());
        buf[30..34].copy_from_slice(&total_buy.to_le_bytes());
        buf[34..38].copy_from_slice(&open.to_le_bytes());
        buf[38..42].copy_from_slice(&close.to_le_bytes());
        buf[42..46].copy_from_slice(&high.to_le_bytes());
        buf[46..50].copy_from_slice(&low.to_le_bytes());

        let hdr = PacketHeader {
            response_code: 4,
            message_length: QUOTE_PACKET_SIZE as u16,
            exchange_segment_code: segment,
            security_id,
        };
        (buf, hdr)
    }

    #[test]
    fn test_parse_quote_all_fields() {
        let (buf, hdr) = make_quote_packet(
            EXCHANGE_SEGMENT_NSE_FNO,
            13,
            24500.50,
            100,
            1740556500,
            24450.25,
            5000000,
            2000000,
            3000000,
            24400.0,
            24300.0,
            24550.0,
            24380.0,
        );
        let tick = parse_quote_packet(&buf, &hdr, 888).unwrap();

        assert_eq!(tick.security_id, 13);
        assert_eq!(tick.exchange_segment_code, EXCHANGE_SEGMENT_NSE_FNO);
        assert!((tick.last_traded_price - 24500.50).abs() < 0.01);
        assert_eq!(tick.last_trade_quantity, 100);
        assert_eq!(tick.exchange_timestamp, 1740556500);
        assert_eq!(tick.received_at_nanos, 888);
        assert!((tick.average_traded_price - 24450.25).abs() < 0.01);
        assert_eq!(tick.volume, 5000000);
        assert_eq!(tick.total_sell_quantity, 2000000);
        assert_eq!(tick.total_buy_quantity, 3000000);
        assert!((tick.day_open - 24400.0).abs() < 0.01);
        assert!((tick.day_close - 24300.0).abs() < 0.01);
        assert!((tick.day_high - 24550.0).abs() < 0.01);
        assert!((tick.day_low - 24380.0).abs() < 0.01);
    }

    #[test]
    fn test_parse_quote_oi_fields_zero() {
        let (buf, hdr) = make_quote_packet(
            0, 1, 100.0, 1, 100, 100.0, 1000, 500, 500, 99.0, 98.0, 101.0, 97.0,
        );
        let tick = parse_quote_packet(&buf, &hdr, 0).unwrap();
        assert_eq!(tick.open_interest, 0);
        assert_eq!(tick.oi_day_high, 0);
        assert_eq!(tick.oi_day_low, 0);
    }

    #[test]
    fn test_parse_quote_truncated_at_49() {
        let buf = vec![0u8; 49];
        let hdr = PacketHeader {
            response_code: 4,
            message_length: 50,
            exchange_segment_code: 0,
            security_id: 1,
        };
        let err = parse_quote_packet(&buf, &hdr, 0).unwrap_err();
        match err {
            ParseError::InsufficientBytes { expected, actual } => {
                assert_eq!(expected, 50);
                assert_eq!(actual, 49);
            }
            _ => panic!("wrong error variant"),
        }
    }

    #[test]
    fn test_parse_quote_zero_values() {
        let (buf, hdr) = make_quote_packet(0, 0, 0.0, 0, 0, 0.0, 0, 0, 0, 0.0, 0.0, 0.0, 0.0);
        let tick = parse_quote_packet(&buf, &hdr, 0).unwrap();
        assert_eq!(tick.last_traded_price, 0.0);
        assert_eq!(tick.volume, 0);
    }

    #[test]
    fn test_parse_quote_max_ltq() {
        let (buf, hdr) = make_quote_packet(
            0,
            1,
            100.0,
            u16::MAX,
            100,
            100.0,
            1000,
            500,
            500,
            99.0,
            98.0,
            101.0,
            97.0,
        );
        let tick = parse_quote_packet(&buf, &hdr, 0).unwrap();
        assert_eq!(tick.last_trade_quantity, u16::MAX);
    }

    #[test]
    fn test_parse_quote_max_volume() {
        let (buf, hdr) = make_quote_packet(
            0,
            1,
            100.0,
            1,
            100,
            100.0,
            u32::MAX,
            500,
            500,
            99.0,
            98.0,
            101.0,
            97.0,
        );
        let tick = parse_quote_packet(&buf, &hdr, 0).unwrap();
        assert_eq!(tick.volume, u32::MAX);
    }

    #[test]
    fn test_parse_quote_extra_bytes_ignored() {
        // Buffer larger than 50 bytes — extra bytes should be ignored
        let (mut buf, hdr) = make_quote_packet(
            0, 1, 100.0, 1, 100, 100.0, 1000, 500, 500, 99.0, 98.0, 101.0, 97.0,
        );
        buf.extend_from_slice(&[0xFF; 20]); // extra garbage
        let tick = parse_quote_packet(&buf, &hdr, 0).unwrap();
        assert_eq!(tick.volume, 1000);
    }

    #[test]
    fn test_parse_quote_truncated_at_header() {
        let buf = vec![0u8; 8]; // only header, no body
        let hdr = PacketHeader {
            response_code: 4,
            message_length: 50,
            exchange_segment_code: 0,
            security_id: 1,
        };
        let err = parse_quote_packet(&buf, &hdr, 0).unwrap_err();
        match err {
            ParseError::InsufficientBytes {
                expected: 50,
                actual: 8,
            } => {}
            _ => panic!("wrong error: {err:?}"),
        }
    }
}
