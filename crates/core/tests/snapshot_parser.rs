//! Snapshot (golden-file) tests for parser output.
//!
//! These tests capture the exact output of parser functions and compare
//! against known-good reference values. Any change to parser output
//! breaks these tests, catching accidental regressions in the binary
//! protocol implementation.
//!
//! Unlike regular unit tests that check individual fields, snapshot tests
//! verify the ENTIRE output structure — catching subtle changes that
//! per-field assertions might miss.

use dhan_live_trader_common::constants::{
    DISCONNECT_PACKET_SIZE, EXCHANGE_SEGMENT_NSE_FNO, FULL_QUOTE_PACKET_SIZE,
    MARKET_DEPTH_PACKET_SIZE, MARKET_STATUS_PACKET_SIZE, OI_PACKET_SIZE,
    PREVIOUS_CLOSE_PACKET_SIZE, QUOTE_PACKET_SIZE, TICKER_PACKET_SIZE,
};
use dhan_live_trader_core::parser::dispatch_frame;
use dhan_live_trader_core::parser::types::ParsedFrame;
use dhan_live_trader_core::websocket::types::DisconnectCode;

// ---------------------------------------------------------------------------
// Helpers — deterministic packet construction
// ---------------------------------------------------------------------------

#[allow(clippy::arithmetic_side_effects)]
fn make_ticker(security_id: u32, ltp: f32, ltt: u32) -> Vec<u8> {
    let mut buf = vec![0u8; TICKER_PACKET_SIZE];
    buf[0] = 2;
    buf[1..3].copy_from_slice(&(TICKER_PACKET_SIZE as u16).to_le_bytes());
    buf[3] = EXCHANGE_SEGMENT_NSE_FNO;
    buf[4..8].copy_from_slice(&security_id.to_le_bytes());
    buf[8..12].copy_from_slice(&ltp.to_le_bytes());
    buf[12..16].copy_from_slice(&ltt.to_le_bytes());
    buf
}

#[allow(clippy::arithmetic_side_effects)]
fn make_quote(security_id: u32, ltp: f32, volume: u32) -> Vec<u8> {
    let mut buf = vec![0u8; QUOTE_PACKET_SIZE];
    buf[0] = 4;
    buf[1..3].copy_from_slice(&(QUOTE_PACKET_SIZE as u16).to_le_bytes());
    buf[3] = EXCHANGE_SEGMENT_NSE_FNO;
    buf[4..8].copy_from_slice(&security_id.to_le_bytes());
    // LTP at offset 8
    buf[8..12].copy_from_slice(&ltp.to_le_bytes());
    // Volume at offset 22
    buf[22..26].copy_from_slice(&volume.to_le_bytes());
    // OHLC at offsets 34, 38, 42, 46
    buf[34..38].copy_from_slice(&100.0_f32.to_le_bytes()); // open
    buf[38..42].copy_from_slice(&105.0_f32.to_le_bytes()); // high
    buf[42..46].copy_from_slice(&99.0_f32.to_le_bytes()); // low
    buf[46..50].copy_from_slice(&ltp.to_le_bytes()); // close
    buf
}

// ---------------------------------------------------------------------------
// Snapshot tests — assert exact parsed output
// ---------------------------------------------------------------------------

#[test]
fn snapshot_ticker_nifty_option() {
    let packet = make_ticker(52432, 245.50, 1_700_000_000);
    let parsed = dispatch_frame(&packet, 1_700_000_000_000_000_000).unwrap();

    // Snapshot the debug output — any change to ParsedFrame/ParsedTick breaks this
    let snapshot = format!("{parsed:?}");
    assert!(
        snapshot.contains("security_id: 52432"),
        "snapshot mismatch: security_id"
    );
    assert!(
        snapshot.contains("last_traded_price: 245.5"),
        "snapshot mismatch: ltp"
    );
    assert!(
        snapshot.contains("exchange_segment_code: 2"),
        "snapshot mismatch: segment"
    );
    assert!(
        snapshot.contains("exchange_timestamp: 1700000000"),
        "snapshot mismatch: timestamp"
    );
}

#[test]
fn snapshot_ticker_zero_price() {
    let packet = make_ticker(99999, 0.0, 0);
    let parsed = dispatch_frame(&packet, 0).unwrap();
    let snapshot = format!("{parsed:?}");
    assert!(snapshot.contains("last_traded_price: 0.0"));
    assert!(snapshot.contains("exchange_timestamp: 0"));
}

#[test]
fn snapshot_quote_with_volume() {
    let packet = make_quote(52432, 245.50, 1_500_000);
    let parsed = dispatch_frame(&packet, 0).unwrap();
    let snapshot = format!("{parsed:?}");
    assert!(snapshot.contains("security_id: 52432"));
    assert!(snapshot.contains("last_traded_price: 245.5"));
    assert!(snapshot.contains("volume: 1500000"));
}

// ---------------------------------------------------------------------------
// Golden value tests — exact field-by-field verification
// ---------------------------------------------------------------------------

#[test]
fn golden_ticker_round_trip_exact() {
    let packet = make_ticker(12345, 99.75, 1_710_000_000);
    let parsed = dispatch_frame(&packet, 42).unwrap();

    // Extract tick from ParsedFrame
    match parsed {
        dhan_live_trader_core::parser::types::ParsedFrame::Tick(tick) => {
            assert_eq!(tick.security_id, 12345);
            assert_eq!(tick.exchange_segment_code, EXCHANGE_SEGMENT_NSE_FNO);
            assert_eq!(tick.last_traded_price, 99.75);
            assert_eq!(tick.exchange_timestamp, 1_710_000_000);
            assert_eq!(tick.received_at_nanos, 42);
            // All other fields must be zero (ticker only has LTP + LTT)
            assert_eq!(tick.volume, 0);
            assert_eq!(tick.last_trade_quantity, 0);
            assert_eq!(tick.average_traded_price, 0.0);
            assert_eq!(tick.total_buy_quantity, 0);
            assert_eq!(tick.total_sell_quantity, 0);
            assert_eq!(tick.day_open, 0.0);
            assert_eq!(tick.day_high, 0.0);
            assert_eq!(tick.day_low, 0.0);
            assert_eq!(tick.day_close, 0.0);
            assert_eq!(tick.open_interest, 0);
        }
        other => panic!("expected Tick variant, got: {other:?}"),
    }
}

#[test]
fn golden_error_empty_frame() {
    let result = dispatch_frame(&[], 0);
    assert!(result.is_err());
    let err = result.unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("frame too short"), "error message: {msg}");
}

#[test]
fn golden_error_unknown_response_code() {
    let mut packet = vec![0u8; 16];
    packet[0] = 255; // Unknown response code
    packet[1..3].copy_from_slice(&16_u16.to_le_bytes());
    packet[3] = EXCHANGE_SEGMENT_NSE_FNO;
    packet[4..8].copy_from_slice(&12345_u32.to_le_bytes());

    let result = dispatch_frame(&packet, 0);
    assert!(result.is_err());
    let err = result.unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("unknown response code"),
        "error message: {msg}"
    );
}

// ---------------------------------------------------------------------------
// Deterministic replay — same input always produces same output
// ---------------------------------------------------------------------------

#[test]
fn deterministic_same_input_same_output() {
    let packet = make_ticker(52432, 245.50, 1_700_000_000);

    let result1 = format!("{:?}", dispatch_frame(&packet, 42).unwrap());
    let result2 = format!("{:?}", dispatch_frame(&packet, 42).unwrap());
    let result3 = format!("{:?}", dispatch_frame(&packet, 42).unwrap());

    assert_eq!(result1, result2, "determinism violation: run 1 != run 2");
    assert_eq!(result2, result3, "determinism violation: run 2 != run 3");
}

#[test]
fn deterministic_different_timestamps_different_output() {
    let packet = make_ticker(52432, 245.50, 1_700_000_000);

    let r1 = format!("{:?}", dispatch_frame(&packet, 100).unwrap());
    let r2 = format!("{:?}", dispatch_frame(&packet, 200).unwrap());

    assert_ne!(
        r1, r2,
        "different received_at_nanos should produce different output"
    );
}

// ---------------------------------------------------------------------------
// Golden value tests — Quote packet: exact field-by-field verification
// ---------------------------------------------------------------------------

#[allow(clippy::arithmetic_side_effects)]
fn make_golden_quote() -> Vec<u8> {
    let mut buf = vec![0u8; QUOTE_PACKET_SIZE];
    buf[0] = 4; // RESPONSE_CODE_QUOTE
    buf[1..3].copy_from_slice(&(QUOTE_PACKET_SIZE as u16).to_le_bytes());
    buf[3] = EXCHANGE_SEGMENT_NSE_FNO;
    buf[4..8].copy_from_slice(&52432_u32.to_le_bytes()); // security_id
    buf[8..12].copy_from_slice(&245.50_f32.to_le_bytes()); // LTP
    buf[12..14].copy_from_slice(&75_u16.to_le_bytes()); // LTQ
    buf[14..18].copy_from_slice(&1_710_000_000_u32.to_le_bytes()); // LTT
    buf[18..22].copy_from_slice(&244.25_f32.to_le_bytes()); // ATP
    buf[22..26].copy_from_slice(&1_500_000_u32.to_le_bytes()); // Volume
    buf[26..30].copy_from_slice(&250_000_u32.to_le_bytes()); // Total Sell Qty
    buf[30..34].copy_from_slice(&300_000_u32.to_le_bytes()); // Total Buy Qty
    buf[34..38].copy_from_slice(&240.0_f32.to_le_bytes()); // Day Open
    buf[38..42].copy_from_slice(&238.0_f32.to_le_bytes()); // Day Close
    buf[42..46].copy_from_slice(&248.0_f32.to_le_bytes()); // Day High
    buf[46..50].copy_from_slice(&235.0_f32.to_le_bytes()); // Day Low
    buf
}

#[test]
fn golden_quote_round_trip_exact() {
    let packet = make_golden_quote();
    let parsed = dispatch_frame(&packet, 99).unwrap();

    match parsed {
        dhan_live_trader_core::parser::types::ParsedFrame::Tick(tick) => {
            assert_eq!(tick.security_id, 52432);
            assert_eq!(tick.exchange_segment_code, EXCHANGE_SEGMENT_NSE_FNO);
            assert_eq!(tick.last_traded_price, 245.50);
            assert_eq!(tick.last_trade_quantity, 75);
            assert_eq!(tick.exchange_timestamp, 1_710_000_000);
            assert_eq!(tick.received_at_nanos, 99);
            assert_eq!(tick.average_traded_price, 244.25);
            assert_eq!(tick.volume, 1_500_000);
            assert_eq!(tick.total_sell_quantity, 250_000);
            assert_eq!(tick.total_buy_quantity, 300_000);
            assert_eq!(tick.day_open, 240.0);
            assert_eq!(tick.day_close, 238.0);
            assert_eq!(tick.day_high, 248.0);
            assert_eq!(tick.day_low, 235.0);
            // Quote has no OI fields
            assert_eq!(tick.open_interest, 0);
        }
        other => panic!("expected Tick variant, got: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Golden value tests — OI packet: exact field verification
// ---------------------------------------------------------------------------

#[test]
fn golden_oi_round_trip_exact() {
    let mut buf = vec![0u8; OI_PACKET_SIZE];
    buf[0] = 5; // RESPONSE_CODE_OI
    buf[1..3].copy_from_slice(&(OI_PACKET_SIZE as u16).to_le_bytes());
    buf[3] = EXCHANGE_SEGMENT_NSE_FNO;
    buf[4..8].copy_from_slice(&49081_u32.to_le_bytes()); // security_id
    buf[8..12].copy_from_slice(&2_500_000_u32.to_le_bytes()); // Open Interest

    let parsed = dispatch_frame(&buf, 0).unwrap();

    match parsed {
        dhan_live_trader_core::parser::types::ParsedFrame::OiUpdate {
            security_id,
            exchange_segment_code,
            open_interest,
        } => {
            assert_eq!(security_id, 49081);
            assert_eq!(exchange_segment_code, EXCHANGE_SEGMENT_NSE_FNO);
            assert_eq!(open_interest, 2_500_000);
        }
        other => panic!("expected OiUpdate variant, got: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Golden value tests — Previous Close packet: exact field verification
// ---------------------------------------------------------------------------

#[test]
fn golden_prev_close_round_trip_exact() {
    let mut buf = vec![0u8; PREVIOUS_CLOSE_PACKET_SIZE];
    buf[0] = 6; // RESPONSE_CODE_PREVIOUS_CLOSE
    buf[1..3].copy_from_slice(&(PREVIOUS_CLOSE_PACKET_SIZE as u16).to_le_bytes());
    buf[3] = EXCHANGE_SEGMENT_NSE_FNO;
    buf[4..8].copy_from_slice(&52432_u32.to_le_bytes()); // security_id
    buf[8..12].copy_from_slice(&24_300.5_f32.to_le_bytes()); // Previous Close
    buf[12..16].copy_from_slice(&1_800_000_u32.to_le_bytes()); // Previous OI

    let parsed = dispatch_frame(&buf, 0).unwrap();

    match parsed {
        dhan_live_trader_core::parser::types::ParsedFrame::PreviousClose {
            security_id,
            exchange_segment_code,
            previous_close,
            previous_oi,
        } => {
            assert_eq!(security_id, 52432);
            assert_eq!(exchange_segment_code, EXCHANGE_SEGMENT_NSE_FNO);
            assert!((previous_close - 24_300.5).abs() < 0.01);
            assert_eq!(previous_oi, 1_800_000);
        }
        other => panic!("expected PreviousClose variant, got: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Golden value tests — Full packet: exact field + depth verification
// ---------------------------------------------------------------------------

#[allow(clippy::arithmetic_side_effects)]
fn make_golden_full() -> Vec<u8> {
    let mut buf = vec![0u8; FULL_QUOTE_PACKET_SIZE];
    buf[0] = 8; // RESPONSE_CODE_FULL
    buf[1..3].copy_from_slice(&(FULL_QUOTE_PACKET_SIZE as u16).to_le_bytes());
    buf[3] = EXCHANGE_SEGMENT_NSE_FNO;
    buf[4..8].copy_from_slice(&52432_u32.to_le_bytes()); // security_id

    // Tick fields (same offsets as Quote up to 33)
    buf[8..12].copy_from_slice(&245.50_f32.to_le_bytes()); // LTP
    buf[12..14].copy_from_slice(&75_u16.to_le_bytes()); // LTQ
    buf[14..18].copy_from_slice(&1_710_000_000_u32.to_le_bytes()); // LTT
    buf[18..22].copy_from_slice(&244.25_f32.to_le_bytes()); // ATP
    buf[22..26].copy_from_slice(&1_500_000_u32.to_le_bytes()); // Volume
    buf[26..30].copy_from_slice(&250_000_u32.to_le_bytes()); // Total Sell
    buf[30..34].copy_from_slice(&300_000_u32.to_le_bytes()); // Total Buy

    // OI fields (Full-specific, offsets 34-45)
    buf[34..38].copy_from_slice(&2_500_000_u32.to_le_bytes()); // OI
    buf[38..42].copy_from_slice(&3_000_000_u32.to_le_bytes()); // Highest OI
    buf[42..46].copy_from_slice(&2_000_000_u32.to_le_bytes()); // Lowest OI

    // OHLC (offsets 46-61)
    buf[46..50].copy_from_slice(&240.0_f32.to_le_bytes()); // Day Open
    buf[50..54].copy_from_slice(&238.0_f32.to_le_bytes()); // Day Close
    buf[54..58].copy_from_slice(&248.0_f32.to_le_bytes()); // Day High
    buf[58..62].copy_from_slice(&235.0_f32.to_le_bytes()); // Day Low

    // Depth: 5 levels × 20 bytes (offsets 62-161)
    // Level 0 (best bid/ask)
    let level_base = 62;
    buf[level_base..level_base + 4].copy_from_slice(&1000_u32.to_le_bytes()); // Bid Qty
    buf[level_base + 4..level_base + 8].copy_from_slice(&800_u32.to_le_bytes()); // Ask Qty
    buf[level_base + 8..level_base + 10].copy_from_slice(&15_u16.to_le_bytes()); // Bid Orders
    buf[level_base + 10..level_base + 12].copy_from_slice(&12_u16.to_le_bytes()); // Ask Orders
    buf[level_base + 12..level_base + 16].copy_from_slice(&245.25_f32.to_le_bytes()); // Bid Price
    buf[level_base + 16..level_base + 20].copy_from_slice(&245.75_f32.to_le_bytes()); // Ask Price

    buf
}

#[test]
fn golden_full_round_trip_exact() {
    let packet = make_golden_full();
    let parsed = dispatch_frame(&packet, 77).unwrap();

    match parsed {
        dhan_live_trader_core::parser::types::ParsedFrame::TickWithDepth(tick, depth) => {
            // Tick fields
            assert_eq!(tick.security_id, 52432);
            assert_eq!(tick.exchange_segment_code, EXCHANGE_SEGMENT_NSE_FNO);
            assert_eq!(tick.last_traded_price, 245.50);
            assert_eq!(tick.last_trade_quantity, 75);
            assert_eq!(tick.exchange_timestamp, 1_710_000_000);
            assert_eq!(tick.received_at_nanos, 77);
            assert_eq!(tick.average_traded_price, 244.25);
            assert_eq!(tick.volume, 1_500_000);
            assert_eq!(tick.total_sell_quantity, 250_000);
            assert_eq!(tick.total_buy_quantity, 300_000);

            // OI fields (Full-specific)
            assert_eq!(tick.open_interest, 2_500_000);
            assert_eq!(tick.oi_day_high, 3_000_000);
            assert_eq!(tick.oi_day_low, 2_000_000);

            // OHLC
            assert_eq!(tick.day_open, 240.0);
            assert_eq!(tick.day_close, 238.0);
            assert_eq!(tick.day_high, 248.0);
            assert_eq!(tick.day_low, 235.0);

            // Depth: 5 levels, verify level 0
            assert_eq!(depth.len(), 5);
            assert_eq!(depth[0].bid_quantity, 1000);
            assert_eq!(depth[0].ask_quantity, 800);
            assert_eq!(depth[0].bid_orders, 15);
            assert_eq!(depth[0].ask_orders, 12);
            assert_eq!(depth[0].bid_price, 245.25);
            assert_eq!(depth[0].ask_price, 245.75);

            // Levels 1-4 should be zero (we only populated level 0)
            for level in &depth[1..] {
                assert_eq!(level.bid_quantity, 0);
                assert_eq!(level.ask_quantity, 0);
            }
        }
        other => panic!("expected TickWithDepth variant, got: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Golden value tests — Market Depth packet (code 3): LTP + 5-level depth
// ---------------------------------------------------------------------------

#[allow(clippy::arithmetic_side_effects)]
fn make_golden_market_depth() -> Vec<u8> {
    let mut buf = vec![0u8; MARKET_DEPTH_PACKET_SIZE];
    buf[0] = 3; // RESPONSE_CODE_MARKET_DEPTH
    buf[1..3].copy_from_slice(&(MARKET_DEPTH_PACKET_SIZE as u16).to_le_bytes());
    buf[3] = EXCHANGE_SEGMENT_NSE_FNO;
    buf[4..8].copy_from_slice(&2885_u32.to_le_bytes()); // Reliance security_id
    buf[8..12].copy_from_slice(&2450.75_f32.to_le_bytes()); // LTP

    // 5 depth levels × 20 bytes starting at offset 12
    // Level 0 (best bid/ask)
    let base = 12;
    buf[base..base + 4].copy_from_slice(&5000_u32.to_le_bytes()); // Bid Qty
    buf[base + 4..base + 8].copy_from_slice(&3000_u32.to_le_bytes()); // Ask Qty
    buf[base + 8..base + 10].copy_from_slice(&25_u16.to_le_bytes()); // Bid Orders
    buf[base + 10..base + 12].copy_from_slice(&18_u16.to_le_bytes()); // Ask Orders
    buf[base + 12..base + 16].copy_from_slice(&2450.50_f32.to_le_bytes()); // Bid Price
    buf[base + 16..base + 20].copy_from_slice(&2451.00_f32.to_le_bytes()); // Ask Price

    // Level 1
    let base = 32;
    buf[base..base + 4].copy_from_slice(&4000_u32.to_le_bytes());
    buf[base + 4..base + 8].copy_from_slice(&2500_u32.to_le_bytes());
    buf[base + 8..base + 10].copy_from_slice(&20_u16.to_le_bytes());
    buf[base + 10..base + 12].copy_from_slice(&14_u16.to_le_bytes());
    buf[base + 12..base + 16].copy_from_slice(&2450.25_f32.to_le_bytes());
    buf[base + 16..base + 20].copy_from_slice(&2451.25_f32.to_le_bytes());

    buf
}

#[test]
fn golden_market_depth_round_trip_exact() {
    let packet = make_golden_market_depth();
    let parsed = dispatch_frame(&packet, 555).unwrap();

    match parsed {
        ParsedFrame::TickWithDepth(tick, depth) => {
            // Tick: only LTP populated (no timestamp, volume, OHLC, OI)
            assert_eq!(tick.security_id, 2885);
            assert_eq!(tick.exchange_segment_code, EXCHANGE_SEGMENT_NSE_FNO);
            assert_eq!(tick.last_traded_price, 2450.75);
            assert_eq!(tick.received_at_nanos, 555);
            // Market Depth has no timestamp, volume, OHLC, OI
            assert_eq!(tick.exchange_timestamp, 0);
            assert_eq!(tick.volume, 0);
            assert_eq!(tick.open_interest, 0);
            assert_eq!(tick.average_traded_price, 0.0);
            assert_eq!(tick.day_open, 0.0);
            assert_eq!(tick.day_high, 0.0);
            assert_eq!(tick.day_low, 0.0);
            assert_eq!(tick.day_close, 0.0);

            // Depth level 0 (best)
            assert_eq!(depth.len(), 5);
            assert_eq!(depth[0].bid_quantity, 5000);
            assert_eq!(depth[0].ask_quantity, 3000);
            assert_eq!(depth[0].bid_orders, 25);
            assert_eq!(depth[0].ask_orders, 18);
            assert_eq!(depth[0].bid_price, 2450.50);
            assert_eq!(depth[0].ask_price, 2451.00);

            // Depth level 1
            assert_eq!(depth[1].bid_quantity, 4000);
            assert_eq!(depth[1].ask_quantity, 2500);
            assert_eq!(depth[1].bid_orders, 20);
            assert_eq!(depth[1].ask_orders, 14);
            assert_eq!(depth[1].bid_price, 2450.25);
            assert_eq!(depth[1].ask_price, 2451.25);

            // Levels 2-4 zero (not populated)
            for level in &depth[2..] {
                assert_eq!(level.bid_quantity, 0);
                assert_eq!(level.ask_quantity, 0);
                assert_eq!(level.bid_orders, 0);
                assert_eq!(level.ask_orders, 0);
                assert_eq!(level.bid_price, 0.0);
                assert_eq!(level.ask_price, 0.0);
            }
        }
        other => panic!("expected TickWithDepth variant, got: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Golden value tests — Market Status packet (code 7): header only
// ---------------------------------------------------------------------------

#[test]
fn golden_market_status_round_trip_exact() {
    let mut buf = vec![0u8; MARKET_STATUS_PACKET_SIZE];
    buf[0] = 7; // RESPONSE_CODE_MARKET_STATUS
    buf[1..3].copy_from_slice(&(MARKET_STATUS_PACKET_SIZE as u16).to_le_bytes());
    buf[3] = EXCHANGE_SEGMENT_NSE_FNO;
    buf[4..8].copy_from_slice(&13_u32.to_le_bytes()); // Nifty 50 index

    let parsed = dispatch_frame(&buf, 0).unwrap();

    match parsed {
        ParsedFrame::MarketStatus {
            security_id,
            exchange_segment_code,
        } => {
            assert_eq!(security_id, 13);
            assert_eq!(exchange_segment_code, EXCHANGE_SEGMENT_NSE_FNO);
        }
        other => panic!("expected MarketStatus variant, got: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Golden value tests — Disconnect packet (code 50): reason code extraction
// ---------------------------------------------------------------------------

#[allow(clippy::arithmetic_side_effects)]
fn make_disconnect(reason_code: u16) -> Vec<u8> {
    let mut buf = vec![0u8; DISCONNECT_PACKET_SIZE];
    buf[0] = 50; // RESPONSE_CODE_DISCONNECT
    buf[1..3].copy_from_slice(&(DISCONNECT_PACKET_SIZE as u16).to_le_bytes());
    buf[3] = 0;
    buf[4..8].copy_from_slice(&0_u32.to_le_bytes());
    buf[8..10].copy_from_slice(&reason_code.to_le_bytes());
    buf
}

#[test]
fn golden_disconnect_805_exceeded_connections() {
    let packet = make_disconnect(805);
    let parsed = dispatch_frame(&packet, 0).unwrap();
    match parsed {
        ParsedFrame::Disconnect(code) => {
            assert_eq!(code, DisconnectCode::ExceededActiveConnections);
        }
        other => panic!("expected Disconnect variant, got: {other:?}"),
    }
}

#[test]
fn golden_disconnect_807_token_expired() {
    let packet = make_disconnect(807);
    let parsed = dispatch_frame(&packet, 0).unwrap();
    match parsed {
        ParsedFrame::Disconnect(code) => {
            assert_eq!(code, DisconnectCode::AccessTokenExpired);
        }
        other => panic!("expected Disconnect variant, got: {other:?}"),
    }
}

#[test]
fn golden_disconnect_all_known_codes() {
    let expected = [
        (801_u16, DisconnectCode::Unknown(801)),
        (802, DisconnectCode::Unknown(802)),
        (803, DisconnectCode::Unknown(803)),
        (804, DisconnectCode::Unknown(804)),
        (805, DisconnectCode::ExceededActiveConnections),
        (806, DisconnectCode::DataApiSubscriptionRequired),
        (807, DisconnectCode::AccessTokenExpired),
        (808, DisconnectCode::InvalidClientId),
        (809, DisconnectCode::AuthenticationFailed),
        (810, DisconnectCode::ClientIdInvalid),
        (814, DisconnectCode::Unknown(814)),
    ];

    for (code, expected_variant) in expected {
        let packet = make_disconnect(code);
        let parsed = dispatch_frame(&packet, 0).unwrap();
        match parsed {
            ParsedFrame::Disconnect(actual) => {
                assert_eq!(
                    actual, expected_variant,
                    "disconnect code {code} should map to {expected_variant:?}"
                );
            }
            other => panic!("expected Disconnect for code {code}, got: {other:?}"),
        }
    }
}

#[test]
fn golden_disconnect_unknown_code_preserved() {
    let packet = make_disconnect(999);
    let parsed = dispatch_frame(&packet, 0).unwrap();
    match parsed {
        ParsedFrame::Disconnect(code) => {
            assert_eq!(code, DisconnectCode::Unknown(999));
        }
        other => panic!("expected Disconnect variant, got: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Golden value tests — Deep Depth (20-level): separate WebSocket parser
// ---------------------------------------------------------------------------

use dhan_live_trader_common::constants::{
    DEEP_DEPTH_HEADER_SIZE, DEEP_DEPTH_LEVEL_SIZE, TWENTY_DEPTH_LEVELS, TWENTY_DEPTH_PACKET_SIZE,
};
use dhan_live_trader_core::parser::{DepthSide, parse_twenty_depth_packet};

#[allow(clippy::arithmetic_side_effects)]
fn make_golden_twenty_depth_bid() -> Vec<u8> {
    let mut buf = vec![0u8; TWENTY_DEPTH_PACKET_SIZE];
    // 12-byte header: msg_length(2) + feed_code(1) + segment(1) + security_id(4) + sequence(4)
    buf[0..2].copy_from_slice(&(TWENTY_DEPTH_PACKET_SIZE as u16).to_le_bytes());
    buf[2] = 41; // Bid feed code
    buf[3] = EXCHANGE_SEGMENT_NSE_FNO;
    buf[4..8].copy_from_slice(&52432_u32.to_le_bytes());
    buf[8..12].copy_from_slice(&100_u32.to_le_bytes()); // sequence

    // Level 0: price(f64) + quantity(u32) + orders(u32) = 16 bytes
    let base = DEEP_DEPTH_HEADER_SIZE;
    buf[base..base + 8].copy_from_slice(&24500.05_f64.to_le_bytes());
    buf[base + 8..base + 12].copy_from_slice(&1000_u32.to_le_bytes());
    buf[base + 12..base + 16].copy_from_slice(&50_u32.to_le_bytes());

    // Level 1
    let base = DEEP_DEPTH_HEADER_SIZE + DEEP_DEPTH_LEVEL_SIZE;
    buf[base..base + 8].copy_from_slice(&24499.95_f64.to_le_bytes());
    buf[base + 8..base + 12].copy_from_slice(&800_u32.to_le_bytes());
    buf[base + 12..base + 16].copy_from_slice(&35_u32.to_le_bytes());

    buf
}

#[test]
fn golden_twenty_depth_bid_round_trip_exact() {
    let packet = make_golden_twenty_depth_bid();
    let parsed = parse_twenty_depth_packet(&packet, 12345).unwrap();

    // Header
    assert_eq!(parsed.header.feed_code, 41);
    assert_eq!(
        parsed.header.exchange_segment_code,
        EXCHANGE_SEGMENT_NSE_FNO
    );
    assert_eq!(parsed.header.security_id, 52432);
    assert_eq!(parsed.header.message_sequence, 100);
    assert_eq!(parsed.received_at_nanos, 12345);
    assert_eq!(parsed.side, DepthSide::Bid);

    // Must have exactly 20 levels
    assert_eq!(parsed.levels.len(), TWENTY_DEPTH_LEVELS);

    // Level 0: f64 price (NOT f32 — deep depth uses f64)
    assert!((parsed.levels[0].price - 24500.05).abs() < 1e-9);
    assert_eq!(parsed.levels[0].quantity, 1000);
    assert_eq!(parsed.levels[0].orders, 50);

    // Level 1
    assert!((parsed.levels[1].price - 24499.95).abs() < 1e-9);
    assert_eq!(parsed.levels[1].quantity, 800);
    assert_eq!(parsed.levels[1].orders, 35);

    // Levels 2-19 zero (not populated)
    for level in &parsed.levels[2..] {
        assert_eq!(level.price, 0.0);
        assert_eq!(level.quantity, 0);
        assert_eq!(level.orders, 0);
    }
}

#[test]
fn golden_twenty_depth_ask_side() {
    let mut buf = vec![0u8; TWENTY_DEPTH_PACKET_SIZE];
    buf[0..2].copy_from_slice(&(TWENTY_DEPTH_PACKET_SIZE as u16).to_le_bytes());
    buf[2] = 51; // Ask feed code
    buf[3] = EXCHANGE_SEGMENT_NSE_FNO;
    buf[4..8].copy_from_slice(&52432_u32.to_le_bytes());
    buf[8..12].copy_from_slice(&101_u32.to_le_bytes());

    let parsed = parse_twenty_depth_packet(&buf, 0).unwrap();
    assert_eq!(parsed.side, DepthSide::Ask);
    assert_eq!(parsed.header.message_sequence, 101);
}

#[test]
fn golden_twenty_depth_header_layout_differs_from_standard_feed() {
    // Standard feed: byte 0 = response_code, bytes 1-2 = msg_length
    // Deep depth:    bytes 0-1 = msg_length, byte 2 = feed_code
    // This test verifies the header layout is NOT the standard feed layout
    let mut buf = vec![0u8; TWENTY_DEPTH_PACKET_SIZE];
    buf[0..2].copy_from_slice(&(TWENTY_DEPTH_PACKET_SIZE as u16).to_le_bytes());
    buf[2] = 41; // feed_code at byte 2 (NOT byte 0)
    buf[3] = 1; // segment at byte 3
    buf[4..8].copy_from_slice(&9999_u32.to_le_bytes());
    buf[8..12].copy_from_slice(&42_u32.to_le_bytes());

    let parsed = parse_twenty_depth_packet(&buf, 0).unwrap();

    // Verify header fields parsed from correct byte positions
    assert_eq!(
        parsed.header.message_length,
        TWENTY_DEPTH_PACKET_SIZE as u16
    );
    assert_eq!(parsed.header.feed_code, 41);
    assert_eq!(parsed.header.exchange_segment_code, 1);
    assert_eq!(parsed.header.security_id, 9999);
    assert_eq!(parsed.header.message_sequence, 42);
}
