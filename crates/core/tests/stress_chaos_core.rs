//! Stress and chaos tests for core crate components.
//!
//! Validates that parsers, subscription builders, and pipeline components
//! handle malformed data, extreme loads, and adversarial inputs without
//! panics, data corruption, or incorrect behavior.
//!
//! NOTE: Timing assertions are skipped under instrumented builds
//! (cargo-careful, sanitizers). Set `TV_SKIP_PERF_ASSERTIONS=1` to skip.

#![allow(clippy::unwrap_used, clippy::arithmetic_side_effects)]

use std::time::{Duration, Instant};

/// Returns true when running under instrumented builds (cargo-careful, sanitizers).
fn skip_perf_assertions() -> bool {
    std::env::var("TV_SKIP_PERF_ASSERTIONS").is_ok()
}

// ===========================================================================
// Helpers — packet construction
// ===========================================================================

use tickvault_common::constants::{
    DISCONNECT_PACKET_SIZE, EXCHANGE_SEGMENT_NSE_FNO, FULL_QUOTE_PACKET_SIZE, OI_PACKET_SIZE,
    PREVIOUS_CLOSE_PACKET_SIZE, QUOTE_PACKET_SIZE, TICKER_PACKET_SIZE,
};

fn make_ticker(security_id: u32, ltp: f32, ltt: u32) -> Vec<u8> {
    let mut buf = vec![0u8; TICKER_PACKET_SIZE];
    buf[0] = 2; // RESPONSE_CODE_TICKER
    buf[1..3].copy_from_slice(&(TICKER_PACKET_SIZE as u16).to_le_bytes());
    buf[3] = EXCHANGE_SEGMENT_NSE_FNO;
    buf[4..8].copy_from_slice(&security_id.to_le_bytes());
    buf[8..12].copy_from_slice(&ltp.to_le_bytes());
    buf[12..16].copy_from_slice(&ltt.to_le_bytes());
    buf
}

fn make_quote(security_id: u32, ltp: f32) -> Vec<u8> {
    let mut buf = vec![0u8; QUOTE_PACKET_SIZE];
    buf[0] = 4; // RESPONSE_CODE_QUOTE
    buf[1..3].copy_from_slice(&(QUOTE_PACKET_SIZE as u16).to_le_bytes());
    buf[3] = EXCHANGE_SEGMENT_NSE_FNO;
    buf[4..8].copy_from_slice(&security_id.to_le_bytes());
    buf[8..12].copy_from_slice(&ltp.to_le_bytes());
    buf
}

fn make_full(security_id: u32, ltp: f32) -> Vec<u8> {
    let mut buf = vec![0u8; FULL_QUOTE_PACKET_SIZE];
    buf[0] = 8; // RESPONSE_CODE_FULL
    buf[1..3].copy_from_slice(&(FULL_QUOTE_PACKET_SIZE as u16).to_le_bytes());
    buf[3] = EXCHANGE_SEGMENT_NSE_FNO;
    buf[4..8].copy_from_slice(&security_id.to_le_bytes());
    buf[8..12].copy_from_slice(&ltp.to_le_bytes());
    buf
}

// ===========================================================================
// 1. Parser — malformed data stress tests
// ===========================================================================

mod stress_parser_malformed {
    use super::*;
    use tickvault_core::parser::dispatch_frame;

    #[test]
    fn test_stress_parser_all_zeros_all_sizes() {
        // All-zero buffers of varying sizes should never panic.
        for size in 0..=200 {
            let buf = vec![0u8; size];
            let _ = dispatch_frame(&buf, 0);
        }
    }

    #[test]
    fn test_stress_parser_all_0xff_all_sizes() {
        // All-0xFF buffers should never panic.
        for size in 0..=200 {
            let buf = vec![0xFFu8; size];
            let _ = dispatch_frame(&buf, 0);
        }
    }

    #[test]
    fn test_stress_parser_alternating_bytes() {
        // Alternating 0xAA/0x55 pattern.
        for size in 0..=200 {
            let buf: Vec<u8> = (0..size)
                .map(|i| if i % 2 == 0 { 0xAA } else { 0x55 })
                .collect();
            let _ = dispatch_frame(&buf, 0);
        }
    }

    #[test]
    fn test_stress_parser_every_response_code_with_zeros() {
        // Try every possible response code byte with zeroed payload.
        for code in 0..=255_u8 {
            let mut buf = vec![0u8; 200];
            buf[0] = code;
            buf[1..3].copy_from_slice(&200_u16.to_le_bytes());
            let _ = dispatch_frame(&buf, 0);
        }
    }

    #[test]
    fn test_stress_parser_truncated_at_every_byte() {
        // Valid ticker packet, truncated at every possible length.
        let full_packet = make_ticker(13, 24500.0, 1772073900);
        for len in 0..full_packet.len() {
            let truncated = &full_packet[..len];
            let _ = dispatch_frame(truncated, 0);
        }
    }

    #[test]
    fn test_stress_parser_truncated_quote_at_every_byte() {
        let full_packet = make_quote(13, 24500.0);
        for len in 0..full_packet.len() {
            let truncated = &full_packet[..len];
            let _ = dispatch_frame(truncated, 0);
        }
    }

    #[test]
    fn test_stress_parser_truncated_full_at_every_byte() {
        let full_packet = make_full(13, 24500.0);
        for len in 0..full_packet.len() {
            let truncated = &full_packet[..len];
            let _ = dispatch_frame(truncated, 0);
        }
    }

    #[test]
    fn test_stress_parser_nan_ltp_all_packet_types() {
        // NaN in the LTP field — must parse without panic.
        let ticker = make_ticker(13, f32::NAN, 1772073900);
        assert!(dispatch_frame(&ticker, 0).is_ok());

        let quote = make_quote(13, f32::NAN);
        assert!(dispatch_frame(&quote, 0).is_ok());

        let full = make_full(13, f32::NAN);
        assert!(dispatch_frame(&full, 0).is_ok());
    }

    #[test]
    fn test_stress_parser_infinity_ltp_all_packet_types() {
        let ticker = make_ticker(13, f32::INFINITY, 1772073900);
        assert!(dispatch_frame(&ticker, 0).is_ok());

        let quote = make_quote(13, f32::INFINITY);
        assert!(dispatch_frame(&quote, 0).is_ok());

        let full = make_full(13, f32::INFINITY);
        assert!(dispatch_frame(&full, 0).is_ok());
    }

    #[test]
    fn test_stress_parser_neg_infinity_ltp() {
        let ticker = make_ticker(13, f32::NEG_INFINITY, 1772073900);
        assert!(dispatch_frame(&ticker, 0).is_ok());
    }

    #[test]
    fn test_stress_parser_max_security_id() {
        let ticker = make_ticker(u32::MAX, 100.0, 1772073900);
        let result = dispatch_frame(&ticker, 0);
        assert!(result.is_ok());
    }

    #[test]
    fn test_stress_parser_zero_security_id() {
        let ticker = make_ticker(0, 100.0, 1772073900);
        let result = dispatch_frame(&ticker, 0);
        assert!(result.is_ok());
    }

    #[test]
    fn test_stress_parser_max_ltt() {
        let ticker = make_ticker(13, 100.0, u32::MAX);
        let result = dispatch_frame(&ticker, 0);
        assert!(result.is_ok());
    }

    #[test]
    fn test_stress_parser_negative_received_at() {
        let ticker = make_ticker(13, 100.0, 1772073900);
        let result = dispatch_frame(&ticker, i64::MIN);
        assert!(result.is_ok());
    }

    #[test]
    fn test_stress_parser_oi_packet_malformed() {
        // OI packet (code 5) = 12 bytes. Try with all zeros.
        let mut buf = vec![0u8; OI_PACKET_SIZE];
        buf[0] = 5; // RESPONSE_CODE_OI
        buf[1..3].copy_from_slice(&(OI_PACKET_SIZE as u16).to_le_bytes());
        assert!(dispatch_frame(&buf, 0).is_ok());
    }

    #[test]
    fn test_stress_parser_prev_close_packet_malformed() {
        // Previous close (code 6) = 16 bytes.
        let mut buf = vec![0u8; PREVIOUS_CLOSE_PACKET_SIZE];
        buf[0] = 6; // RESPONSE_CODE_PREVIOUS_CLOSE
        buf[1..3].copy_from_slice(&(PREVIOUS_CLOSE_PACKET_SIZE as u16).to_le_bytes());
        assert!(dispatch_frame(&buf, 0).is_ok());
    }

    #[test]
    fn test_stress_parser_disconnect_packet_malformed() {
        // Disconnect (code 50) = 10 bytes.
        let mut buf = vec![0u8; DISCONNECT_PACKET_SIZE];
        buf[0] = 50; // RESPONSE_CODE_DISCONNECT
        buf[1..3].copy_from_slice(&(DISCONNECT_PACKET_SIZE as u16).to_le_bytes());
        assert!(dispatch_frame(&buf, 0).is_ok());
    }

    #[test]
    fn test_stress_parser_market_status_packet() {
        // Market status (code 7) = 8 bytes (header only).
        let mut buf = vec![0u8; 8];
        buf[0] = 7; // RESPONSE_CODE_MARKET_STATUS
        buf[1..3].copy_from_slice(&8_u16.to_le_bytes());
        assert!(dispatch_frame(&buf, 0).is_ok());
    }
}

// ===========================================================================
// 2. Parser — sustained throughput
// ===========================================================================

mod stress_parser_throughput {
    use super::*;
    use tickvault_core::parser::dispatch_frame;

    #[test]
    fn test_stress_parse_500k_mixed_packets() {
        let start = Instant::now();
        let mut success = 0_u64;
        let mut errors = 0_u64;

        for i in 0..500_000_u32 {
            let packet = match i % 3 {
                0 => make_ticker(i % 25000, 100.0 + (i as f32 * 0.01), 1772073900 + i),
                1 => make_quote(i % 25000, 200.0 + (i as f32 * 0.01)),
                2 => make_full(i % 25000, 300.0 + (i as f32 * 0.01)),
                _ => unreachable!(),
            };
            match dispatch_frame(&packet, i as i64) {
                Ok(_) => success += 1,
                Err(_) => errors += 1,
            }
        }

        assert_eq!(success, 500_000);
        assert_eq!(errors, 0);

        let elapsed = start.elapsed();
        if !skip_perf_assertions() {
            assert!(
                elapsed < Duration::from_secs(5),
                "500K mixed packet parsing took {elapsed:?}"
            );
        }
    }

    #[test]
    fn test_stress_parse_1m_tickers_throughput() {
        let start = Instant::now();

        for i in 0..1_000_000_u32 {
            let packet = make_ticker(i % 5000, 24500.0 + (i as f32 * 0.001), 1772073900 + i);
            let _ = dispatch_frame(&packet, i as i64);
        }

        let elapsed = start.elapsed();
        if !skip_perf_assertions() {
            assert!(
                elapsed < Duration::from_secs(5),
                "1M ticker parsing took {elapsed:?}"
            );
        }
    }
}

// ===========================================================================
// 3. Subscription Builder — edge cases
// ===========================================================================

mod stress_subscription_builder {
    use tickvault_common::types::{ExchangeSegment, FeedMode};
    use tickvault_core::websocket::subscription_builder::{
        build_disconnect_message, build_subscription_messages, build_unsubscription_messages,
    };
    use tickvault_core::websocket::types::InstrumentSubscription;

    fn make_instruments(count: usize) -> Vec<InstrumentSubscription> {
        (0..count)
            .map(|i| InstrumentSubscription::new(ExchangeSegment::NseFno, (50000 + i) as u32))
            .collect()
    }

    #[test]
    fn test_stress_subscription_builder_zero_instruments() {
        let messages = build_subscription_messages(&[], FeedMode::Full, 100);
        assert!(
            messages.is_empty(),
            "empty instrument list must produce no messages"
        );
    }

    #[test]
    fn test_stress_subscription_builder_single_instrument() {
        let instruments = make_instruments(1);
        let messages = build_subscription_messages(&instruments, FeedMode::Ticker, 100);
        assert_eq!(messages.len(), 1);
        assert!(messages[0].contains("50000"));
    }

    #[test]
    fn test_stress_subscription_builder_exactly_100() {
        let instruments = make_instruments(100);
        let messages = build_subscription_messages(&instruments, FeedMode::Quote, 100);
        assert_eq!(messages.len(), 1, "100 instruments should fit in 1 message");
    }

    #[test]
    fn test_stress_subscription_builder_101_instruments() {
        let instruments = make_instruments(101);
        let messages = build_subscription_messages(&instruments, FeedMode::Full, 100);
        assert_eq!(
            messages.len(),
            2,
            "101 instruments should split into 2 messages"
        );
    }

    #[test]
    fn test_stress_subscription_builder_5000_instruments() {
        let instruments = make_instruments(5000);
        let messages = build_subscription_messages(&instruments, FeedMode::Ticker, 100);
        assert_eq!(messages.len(), 50, "5000 / 100 = 50 messages");
    }

    #[test]
    fn test_stress_subscription_builder_25000_instruments() {
        let instruments = make_instruments(25000);
        let messages = build_subscription_messages(&instruments, FeedMode::Quote, 100);
        assert_eq!(messages.len(), 250, "25000 / 100 = 250 messages");

        // Every message should be valid JSON.
        for (i, msg) in messages.iter().enumerate() {
            assert!(
                serde_json::from_str::<serde_json::Value>(msg).is_ok(),
                "message {i} is invalid JSON: {msg}"
            );
        }
    }

    #[test]
    fn test_stress_subscription_builder_batch_clamped_to_1() {
        let instruments = make_instruments(5);
        // Batch size 0 should be clamped to 1.
        let messages = build_subscription_messages(&instruments, FeedMode::Ticker, 0);
        assert_eq!(
            messages.len(),
            5,
            "batch_size=0 clamped to 1 means 5 messages"
        );
    }

    #[test]
    fn test_stress_subscription_builder_batch_clamped_to_max() {
        let instruments = make_instruments(200);
        // Batch size 500 should be clamped to 100 (SUBSCRIPTION_BATCH_SIZE).
        let messages = build_subscription_messages(&instruments, FeedMode::Ticker, 500);
        assert_eq!(
            messages.len(),
            2,
            "batch_size=500 clamped to 100 means 200/100 = 2 messages"
        );
    }

    #[test]
    fn test_stress_subscription_builder_security_id_is_string_in_json() {
        let instruments = make_instruments(1);
        let messages = build_subscription_messages(&instruments, FeedMode::Ticker, 100);
        // SecurityId must be a string in JSON, not a number.
        assert!(
            messages[0].contains("\"50000\""),
            "SecurityId must be serialized as string: {}",
            messages[0]
        );
    }

    #[test]
    fn test_stress_subscription_builder_unsubscribe_messages() {
        let instruments = make_instruments(50);
        let messages = build_unsubscription_messages(&instruments, FeedMode::Full, 100);
        assert_eq!(messages.len(), 1);
        // Unsubscribe for Full mode uses RequestCode 22.
        assert!(
            messages[0].contains("22"),
            "unsubscribe Full code should be 22"
        );
    }

    #[test]
    fn test_stress_subscription_builder_all_feed_modes() {
        let instruments = make_instruments(10);

        let ticker_msgs = build_subscription_messages(&instruments, FeedMode::Ticker, 100);
        let quote_msgs = build_subscription_messages(&instruments, FeedMode::Quote, 100);
        let full_msgs = build_subscription_messages(&instruments, FeedMode::Full, 100);

        // Ticker=15, Quote=17, Full=21
        assert!(
            ticker_msgs[0].contains("15"),
            "Ticker subscribe code should be 15"
        );
        assert!(
            quote_msgs[0].contains("17"),
            "Quote subscribe code should be 17"
        );
        assert!(
            full_msgs[0].contains("21"),
            "Full subscribe code should be 21"
        );
    }

    #[test]
    fn test_stress_subscription_builder_disconnect_message() {
        let msg = build_disconnect_message();
        assert!(msg.contains("12"), "disconnect code should be 12");
        assert!(
            serde_json::from_str::<serde_json::Value>(&msg).is_ok(),
            "disconnect message must be valid JSON"
        );
    }

    #[test]
    fn test_stress_subscription_builder_unsubscribe_zero_instruments() {
        let messages = build_unsubscription_messages(&[], FeedMode::Ticker, 100);
        assert!(messages.is_empty());
    }

    #[test]
    fn test_stress_subscription_builder_instrument_count_matches() {
        let instruments = make_instruments(73);
        let messages = build_subscription_messages(&instruments, FeedMode::Ticker, 100);
        assert_eq!(messages.len(), 1);

        // Parse and verify InstrumentCount matches actual list length.
        let parsed: serde_json::Value = serde_json::from_str(&messages[0]).unwrap();
        let count = parsed["InstrumentCount"].as_u64().unwrap();
        assert_eq!(count, 73);
    }
}

// ===========================================================================
// 4. Parser — specific packet type stress
// ===========================================================================

mod stress_parser_specific {
    use super::*;
    use tickvault_core::parser::dispatch_frame;
    use tickvault_core::parser::types::ParsedFrame;

    #[test]
    fn test_stress_parser_ticker_field_preservation() {
        // Verify that parsed fields match input across 10K packets.
        for i in 0..10_000_u32 {
            let ltp = 100.0 + (i as f32 * 0.05);
            let ltt = 1772073900 + i;
            let sid = i % 5000;
            let packet = make_ticker(sid, ltp, ltt);
            let result = dispatch_frame(&packet, i as i64).unwrap();

            match result {
                ParsedFrame::Tick(tick) => {
                    assert_eq!(tick.security_id, sid, "security_id mismatch at i={i}");
                    assert!(
                        (tick.last_traded_price - ltp).abs() < f32::EPSILON,
                        "ltp mismatch at i={i}: expected {ltp}, got {}",
                        tick.last_traded_price
                    );
                    assert_eq!(tick.exchange_timestamp, ltt, "ltt mismatch at i={i}");
                    assert_eq!(tick.received_at_nanos, i as i64);
                }
                other => panic!("expected Tick, got {other:?}"),
            }
        }
    }

    #[test]
    fn test_stress_parser_quote_field_preservation() {
        for i in 0..10_000_u32 {
            let ltp = 200.0 + (i as f32 * 0.01);
            let sid = i % 5000;
            let packet = make_quote(sid, ltp);
            let result = dispatch_frame(&packet, 0).unwrap();

            match result {
                ParsedFrame::Tick(tick) => {
                    assert_eq!(tick.security_id, sid);
                    assert!(
                        (tick.last_traded_price - ltp).abs() < f32::EPSILON,
                        "ltp mismatch at i={i}"
                    );
                }
                other => panic!("expected Tick, got {other:?}"),
            }
        }
    }

    #[test]
    fn test_stress_parser_full_produces_depth() {
        let packet = make_full(13, 24500.0);
        let result = dispatch_frame(&packet, 0).unwrap();

        match result {
            ParsedFrame::TickWithDepth(tick, depth) => {
                assert_eq!(tick.security_id, 13);
                assert_eq!(depth.len(), 5);
            }
            other => panic!("expected TickWithDepth, got {other:?}"),
        }
    }

    #[test]
    fn test_stress_parser_all_exchange_segments() {
        // All valid segment bytes should parse correctly in ticker packets.
        let valid_segments = [0_u8, 1, 2, 3, 4, 5, 7, 8];
        for &seg in &valid_segments {
            let mut buf = vec![0u8; TICKER_PACKET_SIZE];
            buf[0] = 2;
            buf[1..3].copy_from_slice(&(TICKER_PACKET_SIZE as u16).to_le_bytes());
            buf[3] = seg;
            buf[4..8].copy_from_slice(&42_u32.to_le_bytes());
            buf[8..12].copy_from_slice(&100.0_f32.to_le_bytes());
            buf[12..16].copy_from_slice(&1772073900_u32.to_le_bytes());

            let result = dispatch_frame(&buf, 0);
            assert!(result.is_ok(), "segment {seg} should parse successfully");
        }
    }
}

// ===========================================================================
// 5. Disconnect code stress
// ===========================================================================

mod stress_disconnect_codes {
    use tickvault_core::websocket::types::DisconnectCode;

    #[test]
    fn test_stress_disconnect_code_from_all_u16() {
        // Every u16 value should produce a valid DisconnectCode (or Unknown).
        // Must never panic.
        for code in 0..=u16::MAX {
            let dc = DisconnectCode::from_u16(code);
            let _ = format!("{dc:?}"); // Debug must not panic
        }
    }

    #[test]
    fn test_stress_disconnect_code_known_roundtrip() {
        let known_codes: [(u16, DisconnectCode); 12] = [
            (800, DisconnectCode::InternalServerError),
            (804, DisconnectCode::InstrumentsExceedLimit),
            (805, DisconnectCode::ExceededActiveConnections),
            (806, DisconnectCode::DataApiSubscriptionRequired),
            (807, DisconnectCode::AccessTokenExpired),
            (808, DisconnectCode::AuthenticationFailed),
            (809, DisconnectCode::AccessTokenInvalid),
            (810, DisconnectCode::ClientIdInvalid),
            (811, DisconnectCode::InvalidExpiryDate),
            (812, DisconnectCode::InvalidDateFormat),
            (813, DisconnectCode::InvalidSecurityId),
            (814, DisconnectCode::InvalidRequest),
        ];

        for (code, expected) in &known_codes {
            let dc = DisconnectCode::from_u16(*code);
            assert_eq!(dc, *expected, "code {code} mismatch");
            assert_eq!(dc.as_u16(), *code, "roundtrip failed for {code}");
        }
    }

    #[test]
    fn test_stress_disconnect_code_only_807_requires_refresh() {
        // Only AccessTokenExpired (807) requires token refresh.
        let known_codes = [
            800_u16, 804, 805, 806, 807, 808, 809, 810, 811, 812, 813, 814,
        ];
        for code in known_codes {
            let dc = DisconnectCode::from_u16(code);
            if code == 807 {
                assert!(
                    dc.requires_token_refresh(),
                    "807 must require token refresh"
                );
            } else {
                assert!(
                    !dc.requires_token_refresh(),
                    "code {code} must NOT require token refresh"
                );
            }
        }
    }
}

// ===========================================================================
// 6. Connection state
// ===========================================================================

mod stress_connection_state {
    use tickvault_core::websocket::types::ConnectionState;

    #[test]
    fn test_stress_connection_state_all_variants_display() {
        let states = [
            ConnectionState::Disconnected,
            ConnectionState::Connecting,
            ConnectionState::Connected,
            ConnectionState::Reconnecting,
        ];
        for state in &states {
            let display = format!("{state}");
            assert!(
                !display.is_empty(),
                "Display must not be empty for {state:?}"
            );
        }
    }

    #[test]
    fn test_stress_connection_state_all_distinct() {
        let states = [
            ConnectionState::Disconnected,
            ConnectionState::Connecting,
            ConnectionState::Connected,
            ConnectionState::Reconnecting,
        ];
        for i in 0..states.len() {
            for j in (i + 1)..states.len() {
                assert_ne!(states[i], states[j], "{:?} == {:?}", states[i], states[j]);
            }
        }
    }

    #[test]
    fn test_stress_connection_state_display_readable() {
        assert_eq!(format!("{}", ConnectionState::Disconnected), "Disconnected");
        assert_eq!(format!("{}", ConnectionState::Connecting), "Connecting");
        assert_eq!(format!("{}", ConnectionState::Connected), "Connected");
        assert_eq!(format!("{}", ConnectionState::Reconnecting), "Reconnecting");
    }
}

// ===========================================================================
// 7. Parser header — boundary stress
// ===========================================================================

mod stress_parser_header {
    use tickvault_core::parser::dispatch_frame;

    #[test]
    fn test_stress_header_every_length_0_to_200() {
        // Must never panic regardless of input length.
        for len in 0..=200 {
            let buf = vec![2u8; len]; // response code 2 = ticker
            let _ = dispatch_frame(&buf, 0);
        }
    }

    #[test]
    fn test_stress_header_empty_buffer() {
        let result = dispatch_frame(&[], 0);
        assert!(result.is_err());
    }

    #[test]
    fn test_stress_header_single_byte() {
        let result = dispatch_frame(&[2], 0);
        assert!(result.is_err());
    }

    #[test]
    fn test_stress_header_exactly_8_bytes_with_ticker_code() {
        // 8 bytes = valid header, but ticker needs 16 bytes total.
        let buf = vec![2u8; 8];
        let result = dispatch_frame(&buf, 0);
        assert!(result.is_err(), "8 bytes is not enough for a ticker packet");
    }
}

// ===========================================================================
// 8. Cross-component: parser + type safety
// ===========================================================================

mod stress_cross_component {
    use super::*;
    use tickvault_core::parser::dispatch_frame;
    use tickvault_core::parser::types::ParsedFrame;

    #[test]
    fn test_stress_parser_extreme_f32_values() {
        let extreme_values = [
            f32::MIN_POSITIVE,
            f32::EPSILON,
            1.0e-38_f32,
            1.0e38_f32,
            -0.0_f32,
        ];

        for &val in &extreme_values {
            let packet = make_ticker(1, val, 1772073900);
            let result = dispatch_frame(&packet, 0);
            assert!(result.is_ok(), "extreme f32 value {val} must parse");
        }
    }

    #[test]
    fn test_stress_parser_sequential_security_ids() {
        // Parse 25K different security IDs.
        for sid in 0..25_000_u32 {
            let packet = make_ticker(sid, 100.0, 1772073900);
            let result = dispatch_frame(&packet, 0).unwrap();
            match result {
                ParsedFrame::Tick(tick) => {
                    assert_eq!(tick.security_id, sid);
                }
                other => panic!("expected Tick, got {other:?} for sid={sid}"),
            }
        }
    }

    #[test]
    fn test_stress_received_at_nanos_range() {
        // Various received_at_nanos values including edge cases.
        let values = [0_i64, 1, -1, i64::MAX, i64::MIN, 1_000_000_000_000_000_000];
        for &nanos in &values {
            let packet = make_ticker(1, 100.0, 1772073900);
            let result = dispatch_frame(&packet, nanos).unwrap();
            match result {
                ParsedFrame::Tick(tick) => {
                    assert_eq!(tick.received_at_nanos, nanos);
                }
                other => panic!("expected Tick, got {other:?}"),
            }
        }
    }
}
