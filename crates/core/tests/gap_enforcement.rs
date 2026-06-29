//! Gap enforcement integration tests.
//!
//! Exhaustive tests for gap implementations across all Core crate domains:
//!
//! ## Instruments
//! - I-P1-01: Daily scheduler timing, boundaries, config
//!
//! ## Network
//! - GAP-NET-01: IP monitor config, comparison, validation (pure functions)
//!
//! ## WebSocket
//! - WS-GAP-01: Disconnect code classification (reconnectable, token refresh, roundtrip)
//! - WS-GAP-02: Subscription batching (batch size clamping, empty input, SecurityId as string)
//! - WS-GAP-03: Connection state machine (4 states, display, equality)
//!
//! Each gap section tests:
//! - Happy path
//! - Error paths
//! - Boundary conditions
//! - Edge cases
//! - Config validation
//!
//! Note: Async tests using tokio_util::CancellationToken and spawn_ip_monitor
//! live in the ip_monitor module's unit tests (uses crate-internal deps).
//! GAP-SEC-01 tests live in crates/api/tests/auth_middleware.rs.
//! AUTH-GAP-01/02 tests live in the auth module's unit tests (require secrecy crate).

#![allow(clippy::assertions_on_constants)]

// ===========================================================================

mod ip_monitor {
    use tickvault_core::network::ip_monitor::*;

    // -- compare_ips: all variants -------------------------------------------

    #[test]
    fn compare_match() {
        assert_eq!(
            compare_ips("203.0.113.42", "203.0.113.42"),
            IpCheckResult::Match
        );
    }

    #[test]
    fn compare_mismatch() {
        let result = compare_ips("1.2.3.4", "5.6.7.8");
        assert!(matches!(result, IpCheckResult::Mismatch { .. }));
        if let IpCheckResult::Mismatch { expected, actual } = result {
            assert_eq!(expected, "1.2.3.4");
            assert_eq!(actual, "5.6.7.8");
        }
    }

    #[test]
    fn compare_empty_strings_match() {
        assert_eq!(compare_ips("", ""), IpCheckResult::Match);
    }

    #[test]
    fn compare_one_empty_mismatch() {
        assert!(matches!(
            compare_ips("1.2.3.4", ""),
            IpCheckResult::Mismatch { .. }
        ));
    }

    #[test]
    fn compare_whitespace_sensitive() {
        assert!(matches!(
            compare_ips("1.2.3.4", "1.2.3.4 "),
            IpCheckResult::Mismatch { .. }
        ));
    }

    #[test]
    fn compare_identical_long_strings() {
        assert_eq!(
            compare_ips("255.255.255.255", "255.255.255.255"),
            IpCheckResult::Match
        );
    }

    // -- is_valid_ipv4: valid ------------------------------------------------

    #[test]
    fn ipv4_valid_standard() {
        assert!(is_valid_ipv4("192.168.1.1"));
        assert!(is_valid_ipv4("10.0.0.1"));
        assert!(is_valid_ipv4("172.16.0.1"));
    }

    #[test]
    fn ipv4_valid_boundaries() {
        assert!(is_valid_ipv4("0.0.0.0"));
        assert!(is_valid_ipv4("255.255.255.255"));
        assert!(is_valid_ipv4("1.1.1.1"));
    }

    // -- is_valid_ipv4: invalid ----------------------------------------------

    #[test]
    fn ipv4_invalid_empty() {
        assert!(!is_valid_ipv4(""));
    }

    #[test]
    fn ipv4_invalid_text() {
        assert!(!is_valid_ipv4("not-an-ip"));
        assert!(!is_valid_ipv4("abc.def.ghi.jkl"));
    }

    #[test]
    fn ipv4_invalid_overflow() {
        assert!(!is_valid_ipv4("256.0.0.1"));
        assert!(!is_valid_ipv4("0.0.0.256"));
        assert!(!is_valid_ipv4("999.999.999.999"));
    }

    #[test]
    fn ipv4_invalid_too_few_octets() {
        assert!(!is_valid_ipv4("1.2.3"));
        assert!(!is_valid_ipv4("1.2"));
        assert!(!is_valid_ipv4("1"));
    }

    #[test]
    fn ipv4_invalid_ipv6() {
        assert!(!is_valid_ipv4("::1"));
        assert!(!is_valid_ipv4("fe80::1"));
    }

    #[test]
    fn ipv4_invalid_with_port() {
        assert!(!is_valid_ipv4("1.2.3.4:8080"));
    }

    #[test]
    fn ipv4_invalid_with_whitespace() {
        assert!(!is_valid_ipv4(" 1.2.3.4"));
        assert!(!is_valid_ipv4("1.2.3.4 "));
        assert!(!is_valid_ipv4("1.2.3.4\n"));
        assert!(!is_valid_ipv4("\t1.2.3.4"));
    }

    #[test]
    fn ipv4_invalid_negative() {
        assert!(!is_valid_ipv4("-1.0.0.0"));
    }

    // -- IpCheckResult: traits -----------------------------------------------

    #[test]
    fn ip_check_result_clone_eq() {
        let a = IpCheckResult::Match;
        let b = a.clone();
        assert_eq!(a, b);
    }

    #[test]
    fn ip_check_result_mismatch_preserves() {
        let r = IpCheckResult::Mismatch {
            expected: "a".to_owned(),
            actual: "b".to_owned(),
        };
        let cloned = r.clone();
        assert_eq!(r, cloned);
    }

    #[test]
    fn ip_check_result_check_failed_debug() {
        let r = IpCheckResult::CheckFailed {
            reason: "timeout".to_owned(),
        };
        let dbg = format!("{r:?}");
        assert!(dbg.contains("timeout"));
    }

    // -- IpMonitorConfig: construction ---------------------------------------

    #[test]
    fn config_new_enabled() {
        let config = IpMonitorConfig::new("1.2.3.4".to_owned(), 60);
        assert!(config.enabled);
        assert_eq!(config.expected_ip, "1.2.3.4");
        assert_eq!(config.check_interval_secs, 60);
    }

    #[test]
    fn config_disabled() {
        let config = IpMonitorConfig::disabled();
        assert!(!config.enabled);
        assert!(config.expected_ip.is_empty());
        assert_eq!(config.check_interval_secs, 300);
    }

    #[test]
    fn config_clone() {
        let config = IpMonitorConfig::new("10.0.0.1".to_owned(), 120);
        let cloned = config.clone();
        assert_eq!(cloned.expected_ip, config.expected_ip);
        assert_eq!(cloned.check_interval_secs, config.check_interval_secs);
        assert_eq!(cloned.enabled, config.enabled);
    }
}

// ===========================================================================
// WS-GAP-01: Disconnect Code Classification
// ===========================================================================

mod ws_disconnect_codes {
    use tickvault_core::websocket::types::DisconnectCode;

    // -- from_u16 ↔ as_u16 roundtrip for all 12 annexure Section 11 codes --

    #[test]
    fn roundtrip_all_known_codes() {
        // All 12 codes from docs/dhan-ref/08-annexure-enums.md Section 11
        let known_codes: &[u16] = &[800, 804, 805, 806, 807, 808, 809, 810, 811, 812, 813, 814];
        for &code in known_codes {
            let dc = DisconnectCode::from_u16(code);
            assert_eq!(dc.as_u16(), code, "roundtrip failed for code {}", code);
        }
    }

    #[test]
    fn unknown_code_roundtrip() {
        let dc = DisconnectCode::from_u16(999);
        assert_eq!(dc.as_u16(), 999);
        assert!(matches!(dc, DisconnectCode::Unknown(999)));
    }

    #[test]
    fn unknown_code_zero_roundtrip() {
        let dc = DisconnectCode::from_u16(0);
        assert_eq!(dc.as_u16(), 0);
    }

    #[test]
    fn codes_801_802_803_not_in_annexure_map_to_unknown() {
        // Codes 801, 802, 803 do NOT exist in annexure Section 11
        for code in [801_u16, 802, 803] {
            let dc = DisconnectCode::from_u16(code);
            assert!(
                matches!(dc, DisconnectCode::Unknown(_)),
                "code {} must map to Unknown (not in annexure)",
                code
            );
        }
    }

    // -- Reconnectable classification per annexure semantics ---------------

    #[test]
    fn reconnectable_codes() {
        // 800 (InternalServerError) and 807 (AccessTokenExpired) are reconnectable
        assert!(DisconnectCode::from_u16(800).is_reconnectable());
        assert!(DisconnectCode::from_u16(807).is_reconnectable());
    }

    #[test]
    fn non_reconnectable_codes() {
        // All config/credential/request errors are NOT reconnectable
        let non_reconnectable: &[u16] = &[804, 805, 806, 808, 809, 810, 811, 812, 813, 814];
        for &code in non_reconnectable {
            assert!(
                !DisconnectCode::from_u16(code).is_reconnectable(),
                "code {} must NOT be reconnectable",
                code
            );
        }
    }

    #[test]
    fn unknown_codes_assume_reconnectable() {
        assert!(DisconnectCode::from_u16(999).is_reconnectable());
        assert!(DisconnectCode::from_u16(0).is_reconnectable());
        assert!(DisconnectCode::from_u16(u16::MAX).is_reconnectable());
    }

    // -- Token refresh: only 807 -----------------------------------------

    #[test]
    fn only_807_requires_token_refresh() {
        // All 12 annexure codes
        let all_codes: &[u16] = &[800, 804, 805, 806, 807, 808, 809, 810, 811, 812, 813, 814];
        for &code in all_codes {
            let dc = DisconnectCode::from_u16(code);
            if code == 807 {
                assert!(
                    dc.requires_token_refresh(),
                    "807 must require token refresh"
                );
            } else {
                assert!(
                    !dc.requires_token_refresh(),
                    "code {} must NOT require token refresh",
                    code
                );
            }
        }
    }

    #[test]
    fn unknown_code_no_token_refresh() {
        assert!(!DisconnectCode::from_u16(999).requires_token_refresh());
    }

    // -- Display produces human-readable strings --------------------------

    #[test]
    fn display_includes_code_number() {
        let dc = DisconnectCode::from_u16(807);
        let s = dc.to_string();
        assert!(s.contains("807"), "display must include code number");
        assert!(
            s.contains("expired") || s.contains("token"),
            "display must describe the error"
        );
    }

    #[test]
    fn display_unknown_includes_code() {
        let dc = DisconnectCode::from_u16(999);
        let s = dc.to_string();
        assert!(s.contains("999"));
    }

    // -- Enum variant coverage (exhaustive) --------------------------------

    #[test]
    fn all_12_known_codes_mapped() {
        // All 12 annexure codes must map to named variants (not Unknown)
        let known: &[(u16, &str)] = &[
            (800, "InternalServerError"),
            (804, "InstrumentsExceedLimit"),
            (805, "ExceededActiveConnections"),
            (806, "DataApiSubscriptionRequired"),
            (807, "AccessTokenExpired"),
            (808, "AuthenticationFailed"),
            (809, "AccessTokenInvalid"),
            (810, "ClientIdInvalid"),
            (811, "InvalidExpiryDate"),
            (812, "InvalidDateFormat"),
            (813, "InvalidSecurityId"),
            (814, "InvalidRequest"),
        ];
        for &(code, expected_name) in known {
            let dc = DisconnectCode::from_u16(code);
            let debug = format!("{:?}", dc);
            assert!(
                debug.contains(expected_name),
                "code {} debug {:?} must contain {}",
                code,
                debug,
                expected_name
            );
        }
        // Codes NOT in annexure (801, 802, 803) map to Unknown
        for code in [801_u16, 802, 803] {
            let dc = DisconnectCode::from_u16(code);
            let debug = format!("{:?}", dc);
            assert!(
                debug.contains("Unknown"),
                "code {} debug {:?} must be Unknown (not in annexure)",
                code,
                debug
            );
        }
    }
}

// ===========================================================================
// WS-GAP-02: Subscription Batching
// ===========================================================================

mod ws_subscription_builder {
    use tickvault_common::constants::{
        FEED_REQUEST_FULL, FEED_REQUEST_QUOTE, FEED_REQUEST_TICKER, FEED_UNSUBSCRIBE_TICKER,
    };
    use tickvault_common::types::{ExchangeSegment, FeedMode};
    use tickvault_core::websocket::subscription_builder::{
        build_disconnect_message, build_subscription_messages, build_unsubscription_messages,
    };
    use tickvault_core::websocket::types::InstrumentSubscription;

    fn make_instruments(count: usize) -> Vec<InstrumentSubscription> {
        (0..count)
            .map(|i| {
                InstrumentSubscription::new(
                    ExchangeSegment::NseFno,
                    1000_u64.saturating_add(i as u64),
                )
            })
            .collect()
    }

    // -- Empty input → empty output ---------------------------------------

    #[test]
    fn empty_instruments_empty_messages() {
        let msgs = build_subscription_messages(&[], FeedMode::Full, 100);
        assert!(msgs.is_empty());
    }

    #[test]
    fn empty_unsubscribe_empty_messages() {
        let msgs = build_unsubscription_messages(&[], FeedMode::Full, 100);
        assert!(msgs.is_empty());
    }

    // -- Single instrument → exactly 1 message ----------------------------

    #[test]
    fn single_instrument_one_message() {
        let instruments = make_instruments(1);
        let msgs = build_subscription_messages(&instruments, FeedMode::Full, 100);
        assert_eq!(msgs.len(), 1);
    }

    // -- Batch size clamping to [1, 100] ----------------------------------

    #[test]
    fn batch_size_clamped_to_1_minimum() {
        let instruments = make_instruments(5);
        let msgs = build_subscription_messages(&instruments, FeedMode::Ticker, 0);
        // batch_size=0 clamped to 1 → 5 messages (1 instrument each)
        assert_eq!(msgs.len(), 5);
    }

    #[test]
    fn batch_size_clamped_to_100_maximum() {
        let instruments = make_instruments(150);
        let msgs = build_subscription_messages(&instruments, FeedMode::Ticker, 200);
        // batch_size=200 clamped to 100 → ceil(150/100) = 2 messages
        assert_eq!(msgs.len(), 2);
    }

    #[test]
    fn exact_batch_size_no_extra_message() {
        let instruments = make_instruments(100);
        let msgs = build_subscription_messages(&instruments, FeedMode::Full, 100);
        assert_eq!(msgs.len(), 1); // exactly 100 fits in 1 batch
    }

    #[test]
    fn batch_boundary_101_instruments() {
        let instruments = make_instruments(101);
        let msgs = build_subscription_messages(&instruments, FeedMode::Full, 100);
        assert_eq!(msgs.len(), 2); // 100 + 1
    }

    // -- SecurityId serialized as string ----------------------------------

    #[test]
    fn security_id_is_string_in_json() {
        let instruments = vec![InstrumentSubscription::new(ExchangeSegment::NseFno, 42)];
        let msgs = build_subscription_messages(&instruments, FeedMode::Full, 100);
        assert_eq!(msgs.len(), 1);

        // Parse JSON and verify SecurityId is a string "42" not number 42
        let json: serde_json::Value = serde_json::from_str(&msgs[0]).unwrap();
        let instrument_list = json["InstrumentList"].as_array().unwrap();
        let security_id = &instrument_list[0]["SecurityId"];
        assert!(security_id.is_string(), "SecurityId must be a JSON string");
        assert_eq!(security_id.as_str().unwrap(), "42");
    }

    // -- Feed mode → request code mapping ---------------------------------

    #[test]
    fn ticker_mode_uses_code_15() {
        let instruments = make_instruments(1);
        let msgs = build_subscription_messages(&instruments, FeedMode::Ticker, 100);
        let json: serde_json::Value = serde_json::from_str(&msgs[0]).unwrap();
        assert_eq!(
            json["RequestCode"].as_u64().unwrap(),
            FEED_REQUEST_TICKER as u64
        );
    }

    #[test]
    fn quote_mode_uses_code_17() {
        let instruments = make_instruments(1);
        let msgs = build_subscription_messages(&instruments, FeedMode::Quote, 100);
        let json: serde_json::Value = serde_json::from_str(&msgs[0]).unwrap();
        assert_eq!(
            json["RequestCode"].as_u64().unwrap(),
            FEED_REQUEST_QUOTE as u64
        );
    }

    #[test]
    fn full_mode_uses_code_21() {
        let instruments = make_instruments(1);
        let msgs = build_subscription_messages(&instruments, FeedMode::Full, 100);
        let json: serde_json::Value = serde_json::from_str(&msgs[0]).unwrap();
        assert_eq!(
            json["RequestCode"].as_u64().unwrap(),
            FEED_REQUEST_FULL as u64
        );
    }

    // -- Unsubscribe codes: subscribe + 1 ---------------------------------

    #[test]
    fn unsubscribe_ticker_uses_code_16() {
        let instruments = make_instruments(1);
        let msgs = build_unsubscription_messages(&instruments, FeedMode::Ticker, 100);
        let json: serde_json::Value = serde_json::from_str(&msgs[0]).unwrap();
        assert_eq!(
            json["RequestCode"].as_u64().unwrap(),
            FEED_UNSUBSCRIBE_TICKER as u64
        );
    }

    // -- InstrumentCount matches batch ------------------------------------

    #[test]
    fn instrument_count_matches_batch() {
        let instruments = make_instruments(5);
        let msgs = build_subscription_messages(&instruments, FeedMode::Full, 3);
        // 5 instruments, batch_size=3 → batches of [3, 2]
        assert_eq!(msgs.len(), 2);

        let json1: serde_json::Value = serde_json::from_str(&msgs[0]).unwrap();
        assert_eq!(json1["InstrumentCount"].as_u64().unwrap(), 3);

        let json2: serde_json::Value = serde_json::from_str(&msgs[1]).unwrap();
        assert_eq!(json2["InstrumentCount"].as_u64().unwrap(), 2);
    }

    // -- Disconnect message -----------------------------------------------

    #[test]
    fn disconnect_message_has_code_12() {
        let msg = build_disconnect_message();
        let json: serde_json::Value = serde_json::from_str(&msg).unwrap();
        assert_eq!(json["RequestCode"].as_u64().unwrap(), 12);
    }
}

// ===========================================================================
// WS-GAP-03: Connection State Machine
// ===========================================================================

mod ws_connection_state {
    use tickvault_core::websocket::types::ConnectionState;

    #[test]
    fn all_four_states_distinct() {
        let states = [
            ConnectionState::Disconnected,
            ConnectionState::Connecting,
            ConnectionState::Connected,
            ConnectionState::Reconnecting,
        ];
        for i in 0..states.len() {
            for j in (i + 1)..states.len() {
                assert_ne!(states[i], states[j], "states must be distinct");
            }
        }
    }

    #[test]
    fn clone_preserves_state() {
        let state = ConnectionState::Connected;
        let cloned = state;
        assert_eq!(state, cloned);
    }

    #[test]
    fn display_produces_human_readable() {
        assert_eq!(ConnectionState::Disconnected.to_string(), "Disconnected");
        assert_eq!(ConnectionState::Connecting.to_string(), "Connecting");
        assert_eq!(ConnectionState::Connected.to_string(), "Connected");
        assert_eq!(ConnectionState::Reconnecting.to_string(), "Reconnecting");
    }

    #[test]
    fn debug_contains_variant_name() {
        let dbg = format!("{:?}", ConnectionState::Connected);
        assert!(dbg.contains("Connected"));
    }
}

mod i_p2_02_trading_day_guard {
    use chrono::NaiveDate;
    use tickvault_common::config::{NseHolidayEntry, TradingConfig};
    use tickvault_common::trading_calendar::TradingCalendar;

    fn make_test_config() -> TradingConfig {
        TradingConfig {
            market_open_time: "09:00:00".to_string(),
            market_close_time: "15:30:00".to_string(),
            order_cutoff_time: "15:29:00".to_string(),
            data_collection_start: "09:00:00".to_string(),
            data_collection_end: "15:30:00".to_string(),
            timezone: "Asia/Kolkata".to_string(),
            max_orders_per_second: 10,
            nse_holidays: vec![
                NseHolidayEntry {
                    date: "2026-01-26".to_string(),
                    name: "Republic Day".to_string(),
                },
                NseHolidayEntry {
                    date: "2026-03-03".to_string(),
                    name: "Holi".to_string(),
                },
                NseHolidayEntry {
                    date: "2026-08-14".to_string(),
                    name: "Independence Day".to_string(),
                },
            ],
            muhurat_trading_dates: vec![],
            nse_mock_trading_dates: vec![],
        }
    }

    // -- Saturday is not a trading day -----------------------------------

    #[test]
    fn test_i_p2_02_saturday_not_trading_day() {
        let config = make_test_config();
        let cal = TradingCalendar::from_config(&config).unwrap();
        // 2026-03-07 is a Saturday
        let saturday = NaiveDate::from_ymd_opt(2026, 3, 7).unwrap(); // APPROVED: test constant
        assert!(
            !cal.is_trading_day(saturday),
            "I-P2-02: Saturday must not be a trading day"
        );
    }

    // -- Sunday is not a trading day -------------------------------------

    #[test]
    fn test_i_p2_02_sunday_not_trading_day() {
        let config = make_test_config();
        let cal = TradingCalendar::from_config(&config).unwrap();
        // 2026-03-08 is a Sunday
        let sunday = NaiveDate::from_ymd_opt(2026, 3, 8).unwrap(); // APPROVED: test constant
        assert!(
            !cal.is_trading_day(sunday),
            "I-P2-02: Sunday must not be a trading day"
        );
    }

    // -- Monday (non-holiday) is a trading day ---------------------------

    #[test]
    fn test_i_p2_02_monday_is_trading_day() {
        let config = make_test_config();
        let cal = TradingCalendar::from_config(&config).unwrap();
        // 2026-03-09 is a Monday (not a holiday)
        let monday = NaiveDate::from_ymd_opt(2026, 3, 9).unwrap(); // APPROVED: test constant
        assert!(
            cal.is_trading_day(monday),
            "I-P2-02: Monday (non-holiday) must be a trading day"
        );
    }

    // -- Known holiday is not a trading day ------------------------------

    #[test]
    fn test_i_p2_02_known_holiday_not_trading_day() {
        let config = make_test_config();
        let cal = TradingCalendar::from_config(&config).unwrap();
        // 2026-01-26 is Republic Day (Monday)
        let republic_day = NaiveDate::from_ymd_opt(2026, 1, 26).unwrap(); // APPROVED: test constant
        assert!(
            !cal.is_trading_day(republic_day),
            "I-P2-02: Republic Day must not be a trading day"
        );
    }

    // -- Holi is not a trading day ---------------------------------------

    #[test]
    fn test_i_p2_02_holi_not_trading_day() {
        let config = make_test_config();
        let cal = TradingCalendar::from_config(&config).unwrap();
        // 2026-03-03 is Holi (Tuesday)
        let holi = NaiveDate::from_ymd_opt(2026, 3, 3).unwrap(); // APPROVED: test constant
        assert!(
            !cal.is_trading_day(holi),
            "I-P2-02: Holi must not be a trading day"
        );
    }

    // -- All weekdays in a non-holiday week are trading days --------------

    #[test]
    fn test_i_p2_02_all_weekdays_trading_in_normal_week() {
        let config = make_test_config();
        let cal = TradingCalendar::from_config(&config).unwrap();
        // Week of 2026-03-09 (Mon) to 2026-03-13 (Fri) — no holidays
        for day in 9..=13_u32 {
            let date = NaiveDate::from_ymd_opt(2026, 3, day).unwrap(); // APPROVED: test constant
            assert!(
                cal.is_trading_day(date),
                "I-P2-02: weekday 2026-03-{:02} must be a trading day",
                day
            );
        }
    }

    // -- Weekend after holiday week still non-trading ---------------------

    #[test]
    fn test_i_p2_02_weekend_detection_independent_of_holidays() {
        let config = make_test_config();
        let cal = TradingCalendar::from_config(&config).unwrap();
        // 2026-01-24 (Saturday) and 2026-01-25 (Sunday) — near Republic Day
        let saturday = NaiveDate::from_ymd_opt(2026, 1, 24).unwrap(); // APPROVED: test constant
        let sunday = NaiveDate::from_ymd_opt(2026, 1, 25).unwrap(); // APPROVED: test constant
        assert!(!cal.is_trading_day(saturday));
        assert!(!cal.is_trading_day(sunday));
    }

    // -- next_trading_day skips weekends and holidays --------------------

    #[test]
    fn test_i_p2_02_next_trading_day_skips_weekend_and_holiday() {
        let config = make_test_config();
        let cal = TradingCalendar::from_config(&config).unwrap();
        // 2026-01-24 (Sat) → skip Sun 25 → skip Mon 26 (Republic Day) → Tue 27
        let saturday = NaiveDate::from_ymd_opt(2026, 1, 24).unwrap(); // APPROVED: test constant
        let expected = NaiveDate::from_ymd_opt(2026, 1, 27).unwrap(); // APPROVED: test constant
        assert_eq!(
            cal.next_trading_day(saturday),
            expected,
            "I-P2-02: next trading day after Sat before holiday must skip both"
        );
    }
}
