//! AB Testing Verification — ensures config constants match Dhan API spec.
//!
//! These tests serve as drift detection: if Dhan changes their API,
//! these tests will catch the discrepancy before it hits production.
//! Also covers market hour boundaries and trading calendar invariants.

#[allow(
    clippy::unwrap_used,
    clippy::arithmetic_side_effects,
    clippy::assertions_on_constants
)] // APPROVED: S4 — schema-stability drift detection tests intentionally assert on compile-time constants as regression guards against Dhan API changes
mod config_drift_detection {
    use dhan_live_trader_common::constants::*;

    /// DRIFT-01: Packet sizes must match Dhan protocol spec.
    #[test]
    fn drift_packet_sizes_match_spec() {
        assert_eq!(BINARY_HEADER_SIZE, 8, "Header must be 8 bytes");
        assert_eq!(TICKER_PACKET_SIZE, 16, "Ticker must be 16 bytes");
        assert_eq!(QUOTE_PACKET_SIZE, 50, "Quote must be 50 bytes");
        assert_eq!(FULL_QUOTE_PACKET_SIZE, 162, "Full must be 162 bytes");
        assert_eq!(OI_PACKET_SIZE, 12, "OI must be 12 bytes");
        assert_eq!(PREVIOUS_CLOSE_PACKET_SIZE, 16, "PrevClose must be 16 bytes");
    }

    /// DRIFT-02: Depth header size (12 bytes, NOT 8).
    #[test]
    fn drift_depth_header_size() {
        assert_eq!(
            DEEP_DEPTH_HEADER_SIZE, 12,
            "Depth header must be 12 bytes (not 8)"
        );
    }

    /// DRIFT-03: Max connections per Dhan API.
    #[test]
    fn drift_max_connections() {
        assert_eq!(
            MAX_WEBSOCKET_CONNECTIONS, 5,
            "Max 5 WebSocket connections per Dhan user"
        );
    }

    /// DRIFT-04: Max instruments per connection.
    #[test]
    fn drift_max_instruments_per_connection() {
        assert_eq!(
            MAX_INSTRUMENTS_PER_WEBSOCKET_CONNECTION, 5000,
            "Max 5000 instruments per connection"
        );
    }

    /// DRIFT-05: Max instruments per subscribe message.
    #[test]
    fn drift_max_instruments_per_message() {
        assert_eq!(
            SUBSCRIPTION_BATCH_SIZE, 100,
            "Max 100 instruments per subscribe message"
        );
    }

    /// DRIFT-06: SEBI order rate limit.
    #[test]
    fn drift_order_rate_limit() {
        assert_eq!(
            SEBI_MAX_ORDERS_PER_SECOND, 10,
            "Order rate limit is 10/sec per Dhan API / SEBI"
        );
    }

    /// DRIFT-07: IST offset constant.
    #[test]
    fn drift_ist_offset() {
        assert_eq!(
            IST_UTC_OFFSET_SECONDS, 19800,
            "IST offset is 5h30m = 19800 seconds"
        );
    }

    /// DRIFT-08: Disconnect packet size.
    #[test]
    fn drift_disconnect_packet_size() {
        assert_eq!(
            DISCONNECT_PACKET_SIZE, 10,
            "Disconnect packet must be 10 bytes"
        );
    }

    /// DRIFT-09: Market depth level count.
    #[test]
    fn drift_depth_levels() {
        assert_eq!(
            MARKET_DEPTH_LEVELS, 5,
            "Live market feed has 5 depth levels"
        );
    }

    /// DRIFT-10: Market depth level size.
    #[test]
    fn drift_depth_level_size() {
        assert_eq!(MARKET_DEPTH_LEVEL_SIZE, 20, "Each depth level is 20 bytes");
    }

    /// DRIFT-11: Full depth level size (f64 not f32).
    #[test]
    fn drift_full_depth_level_size() {
        assert_eq!(
            DEEP_DEPTH_LEVEL_SIZE, 16,
            "Full depth level is 16 bytes (f64 price + u32 qty + u32 orders)"
        );
    }

    /// DRIFT-12: Twenty-depth packet size.
    #[test]
    fn drift_twenty_depth_packet_size() {
        assert_eq!(
            TWENTY_DEPTH_PACKET_SIZE, 332,
            "20-level depth: 12 header + 320 data (20 x 16 bytes)"
        );
    }

    /// DRIFT-13: Market status packet is header-only.
    #[test]
    fn drift_market_status_packet_size() {
        assert_eq!(
            MARKET_STATUS_PACKET_SIZE, 8,
            "Market status packet is just 8-byte header"
        );
    }

    /// DRIFT-14: IST nanos offset.
    #[test]
    fn drift_ist_nanos_offset() {
        assert_eq!(
            IST_UTC_OFFSET_NANOS, 19_800_000_000_000_i64,
            "IST nanos offset must be 19800 * 1e9"
        );
    }

    /// DRIFT-15: Minimum valid exchange timestamp.
    #[test]
    fn drift_minimum_exchange_timestamp() {
        assert!(
            MINIMUM_VALID_EXCHANGE_TIMESTAMP > 946_684_799,
            "Min timestamp should be >= 2000-01-01"
        );
    }

    /// DRIFT-16: Data API error codes match Dhan annexure.
    #[test]
    fn drift_data_api_error_codes() {
        assert_eq!(DATA_API_INTERNAL_SERVER_ERROR, 800);
        assert_eq!(DATA_API_INSTRUMENTS_EXCEED_LIMIT, 804);
        assert_eq!(DATA_API_EXCEEDED_ACTIVE_CONNECTIONS, 805);
        assert_eq!(DATA_API_NOT_SUBSCRIBED, 806);
        assert_eq!(DATA_API_ACCESS_TOKEN_EXPIRED, 807);
        assert_eq!(DATA_API_AUTHENTICATION_FAILED, 808);
        assert_eq!(DATA_API_ACCESS_TOKEN_INVALID, 809);
        assert_eq!(DATA_API_CLIENT_ID_INVALID, 810);
        assert_eq!(DATA_API_INVALID_EXPIRY_DATE, 811);
        assert_eq!(DATA_API_INVALID_DATE_FORMAT, 812);
        assert_eq!(DATA_API_INVALID_SECURITY_ID, 813);
        assert_eq!(DATA_API_INVALID_REQUEST, 814);
    }

    /// DRIFT-17: Disconnect codes alias data API codes correctly.
    #[test]
    fn drift_disconnect_code_aliases() {
        assert_eq!(DISCONNECT_EXCEEDED_ACTIVE_CONNECTIONS, 805);
        assert_eq!(DISCONNECT_DATA_API_SUBSCRIPTION_REQUIRED, 806);
        assert_eq!(DISCONNECT_ACCESS_TOKEN_EXPIRED, 807);
        assert_eq!(DISCONNECT_AUTHENTICATION_FAILED, 808);
        assert_eq!(DISCONNECT_ACCESS_TOKEN_INVALID, 809);
    }

    /// DRIFT-18: OMS circuit breaker threshold matches spec.
    #[test]
    fn drift_oms_circuit_breaker() {
        assert_eq!(
            OMS_CIRCUIT_BREAKER_FAILURE_THRESHOLD, 3,
            "Circuit breaker opens after 3 consecutive failures"
        );
    }

    /// DRIFT-19: SEBI audit retention period.
    #[test]
    fn drift_sebi_audit_retention() {
        assert_eq!(
            SEBI_AUDIT_RETENTION_YEARS, 5,
            "SEBI requires 5 years retention"
        );
    }

    /// DRIFT-20: Twenty depth levels correct.
    #[test]
    fn drift_twenty_depth_levels() {
        assert_eq!(TWENTY_DEPTH_LEVELS, 20);
    }

    /// DRIFT-21: Two hundred depth levels correct.
    #[test]
    fn drift_two_hundred_depth_levels() {
        assert_eq!(TWO_HUNDRED_DEPTH_LEVELS, 200);
    }

    /// DRIFT-22: Deep depth bid/ask feed codes.
    #[test]
    fn drift_deep_depth_feed_codes() {
        assert_eq!(DEEP_DEPTH_FEED_CODE_BID, 41, "Bid feed code");
        assert_eq!(DEEP_DEPTH_FEED_CODE_ASK, 51, "Ask feed code");
    }

    /// DRIFT-23: Header byte offsets match Dhan spec.
    #[test]
    fn drift_header_byte_offsets() {
        assert_eq!(HEADER_OFFSET_RESPONSE_CODE, 0);
        assert_eq!(HEADER_OFFSET_MESSAGE_LENGTH, 1);
        assert_eq!(HEADER_OFFSET_EXCHANGE_SEGMENT, 3);
        assert_eq!(HEADER_OFFSET_SECURITY_ID, 4);
    }

    /// DRIFT-24: Feed response code constants match Dhan spec.
    #[test]
    fn drift_feed_response_codes() {
        assert_eq!(RESPONSE_CODE_TICKER, 2);
        assert_eq!(RESPONSE_CODE_MARKET_DEPTH, 3);
        assert_eq!(RESPONSE_CODE_QUOTE, 4);
        assert_eq!(RESPONSE_CODE_OI, 5);
        assert_eq!(RESPONSE_CODE_PREVIOUS_CLOSE, 6);
        assert_eq!(RESPONSE_CODE_MARKET_STATUS, 7);
        assert_eq!(RESPONSE_CODE_FULL, 8);
        assert_eq!(RESPONSE_CODE_DISCONNECT, 50);
    }

    /// DRIFT-25: Full packet depth starts at byte 62.
    #[test]
    fn drift_full_packet_depth_offset() {
        assert_eq!(FULL_OFFSET_DEPTH_START, 62);
    }

    /// DRIFT-26: Intraday max days per request = 90.
    #[test]
    fn drift_intraday_max_days() {
        assert_eq!(DHAN_INTRADAY_MAX_DAYS_PER_REQUEST, 90);
    }
}

#[allow(
    clippy::unwrap_used,
    clippy::arithmetic_side_effects,
    clippy::assertions_on_constants
)] // APPROVED: S4 schema-drift guards
mod market_hours_verification {
    use chrono::{Datelike, NaiveDate, Weekday};
    use dhan_live_trader_common::trading_calendar::TradingCalendar;

    fn make_calendar() -> TradingCalendar {
        let config = dhan_live_trader_common::config::TradingConfig {
            market_open_time: "09:15:00".to_string(),
            market_close_time: "15:30:00".to_string(),
            order_cutoff_time: "15:29:00".to_string(),
            data_collection_start: "09:00:00".to_string(),
            data_collection_end: "16:00:00".to_string(),
            timezone: "Asia/Kolkata".to_string(),
            max_orders_per_second: 10,
            nse_holidays: vec![],
            muhurat_trading_dates: vec![],
            nse_mock_trading_dates: vec![],
        };
        TradingCalendar::from_config(&config).unwrap()
    }

    /// SIM-01: Regular weekday is a trading day.
    #[test]
    fn sim_weekday_is_trading() {
        let calendar = make_calendar();
        // 2026-03-30 is Monday
        let monday = NaiveDate::from_ymd_opt(2026, 3, 30).unwrap();
        assert!(
            calendar.is_trading_day(monday),
            "Monday should be a trading day"
        );
    }

    /// SIM-02: Saturday is not a trading day.
    #[test]
    fn sim_saturday_not_trading() {
        let calendar = make_calendar();
        let saturday = NaiveDate::from_ymd_opt(2026, 3, 28).unwrap();
        assert_eq!(saturday.weekday(), Weekday::Sat);
        assert!(
            !calendar.is_trading_day(saturday),
            "Saturday should not be a trading day"
        );
    }

    /// SIM-03: Sunday is not a trading day.
    #[test]
    fn sim_sunday_not_trading() {
        let calendar = make_calendar();
        let sunday = NaiveDate::from_ymd_opt(2026, 3, 29).unwrap();
        assert_eq!(sunday.weekday(), Weekday::Sun);
        assert!(
            !calendar.is_trading_day(sunday),
            "Sunday should not be a trading day"
        );
    }

    /// SIM-04: Holiday is not a trading day.
    #[test]
    fn sim_holiday_not_trading() {
        use dhan_live_trader_common::config::{NseHolidayEntry, TradingConfig};

        let config = TradingConfig {
            market_open_time: "09:15:00".to_string(),
            market_close_time: "15:30:00".to_string(),
            order_cutoff_time: "15:29:00".to_string(),
            data_collection_start: "09:00:00".to_string(),
            data_collection_end: "16:00:00".to_string(),
            timezone: "Asia/Kolkata".to_string(),
            max_orders_per_second: 10,
            nse_holidays: vec![NseHolidayEntry {
                date: "2026-01-26".to_string(),
                name: "Republic Day".to_string(),
            }],
            muhurat_trading_dates: vec![],
            nse_mock_trading_dates: vec![],
        };
        let calendar = TradingCalendar::from_config(&config).unwrap();
        let holiday = NaiveDate::from_ymd_opt(2026, 1, 26).unwrap();
        assert!(
            !calendar.is_trading_day(holiday),
            "Republic Day is not a trading day"
        );
        assert!(calendar.is_holiday(holiday));
    }

    /// SIM-05: 375 candles per day (09:15 to 15:30).
    #[test]
    fn sim_candle_count_per_day() {
        let start_minutes = 9 * 60 + 15; // 555
        let end_minutes = 15 * 60 + 30; // 930
        let candle_count = end_minutes - start_minutes;
        assert_eq!(candle_count, 375, "375 one-minute candles per trading day");
    }

    /// SIM-06: next_trading_day from Saturday returns Monday.
    #[test]
    fn sim_next_trading_day_skips_weekend() {
        let calendar = make_calendar();
        let saturday = NaiveDate::from_ymd_opt(2026, 3, 28).unwrap();
        assert_eq!(saturday.weekday(), Weekday::Sat);
        let next = calendar.next_trading_day(saturday);
        assert_eq!(
            next.weekday(),
            Weekday::Mon,
            "Next trading day from Saturday should be Monday"
        );
    }

    /// SIM-07: next_trading_day from Monday returns Monday itself.
    #[test]
    fn sim_next_trading_day_weekday() {
        let calendar = make_calendar();
        let monday = NaiveDate::from_ymd_opt(2026, 3, 30).unwrap();
        assert_eq!(monday.weekday(), Weekday::Mon);
        let next = calendar.next_trading_day(monday);
        assert_eq!(next, monday, "Monday is already a trading day");
    }

    /// SIM-08: Holiday count matches config.
    #[test]
    fn sim_holiday_count() {
        let calendar = make_calendar();
        assert_eq!(
            calendar.holiday_count(),
            0,
            "Empty config has zero holidays"
        );
    }

    /// SIM-09: ist_offset returns +05:30.
    #[test]
    fn sim_ist_offset() {
        let offset = dhan_live_trader_common::trading_calendar::ist_offset();
        assert_eq!(
            offset.local_minus_utc(),
            19800,
            "IST offset should be +19800 seconds"
        );
    }

    /// SIM-10: Calendar from config with empty holidays succeeds.
    #[test]
    fn sim_calendar_construction() {
        let calendar = make_calendar();
        assert_eq!(calendar.holiday_count(), 0);
        assert_eq!(calendar.muhurat_count(), 0);
        assert_eq!(calendar.mock_trading_count(), 0);
    }
}

#[allow(
    clippy::unwrap_used,
    clippy::arithmetic_side_effects,
    clippy::assertions_on_constants
)] // APPROVED: S4 schema-drift guards
mod exchange_segment_ab_tests {
    use dhan_live_trader_common::types::ExchangeSegment;

    /// AB-SEG-01: Exchange segment enum covers all Dhan segments.
    #[test]
    fn ab_all_exchange_segments_exist() {
        let _idx = ExchangeSegment::IdxI;
        let _nse_eq = ExchangeSegment::NseEquity;
        let _nse_fno = ExchangeSegment::NseFno;
        let _nse_currency = ExchangeSegment::NseCurrency;
        let _bse_eq = ExchangeSegment::BseEquity;
        let _mcx_comm = ExchangeSegment::McxComm;
        let _bse_currency = ExchangeSegment::BseCurrency;
        let _bse_fno = ExchangeSegment::BseFno;
    }

    /// AB-SEG-02: Exchange segment binary_code() matches Dhan spec.
    #[test]
    fn ab_exchange_segment_codes() {
        assert_eq!(ExchangeSegment::IdxI.binary_code(), 0);
        assert_eq!(ExchangeSegment::NseEquity.binary_code(), 1);
        assert_eq!(ExchangeSegment::NseFno.binary_code(), 2);
        assert_eq!(ExchangeSegment::NseCurrency.binary_code(), 3);
        assert_eq!(ExchangeSegment::BseEquity.binary_code(), 4);
        assert_eq!(ExchangeSegment::McxComm.binary_code(), 5);
        // Note: NO enum 6 (gap in Dhan spec)
        assert_eq!(ExchangeSegment::BseCurrency.binary_code(), 7);
        assert_eq!(ExchangeSegment::BseFno.binary_code(), 8);
    }

    /// AB-SEG-03: from_byte returns None for enum 6 (gap in spec).
    #[test]
    fn ab_exchange_segment_gap_at_6() {
        assert!(
            ExchangeSegment::from_byte(6).is_none(),
            "Enum 6 must return None — gap in Dhan spec"
        );
    }

    /// AB-SEG-04: from_byte returns None for unknown values.
    #[test]
    fn ab_exchange_segment_unknown() {
        assert!(ExchangeSegment::from_byte(9).is_none());
        assert!(ExchangeSegment::from_byte(255).is_none());
    }

    /// AB-SEG-05: from_byte roundtrips for all valid values.
    #[test]
    fn ab_exchange_segment_roundtrip() {
        for code in [0, 1, 2, 3, 4, 5, 7, 8] {
            let seg = ExchangeSegment::from_byte(code).unwrap();
            assert_eq!(seg.binary_code(), code, "Roundtrip failed for code {code}");
        }
    }
}

#[allow(
    clippy::unwrap_used,
    clippy::arithmetic_side_effects,
    clippy::assertions_on_constants
)] // APPROVED: S4 schema-drift guards
mod order_types_ab_tests {
    use dhan_live_trader_common::order_types::OrderStatus;

    /// AB-ORD-01: All 10 order status variants exist (includes Confirmed).
    #[test]
    fn ab_all_order_statuses() {
        let _transit = OrderStatus::Transit;
        let _pending = OrderStatus::Pending;
        let _confirmed = OrderStatus::Confirmed;
        let _rejected = OrderStatus::Rejected;
        let _cancelled = OrderStatus::Cancelled;
        let _part_traded = OrderStatus::PartTraded;
        let _traded = OrderStatus::Traded;
        let _expired = OrderStatus::Expired;
        let _closed = OrderStatus::Closed;
        let _triggered = OrderStatus::Triggered;
    }

    /// AB-ORD-02: OrderStatus as_str produces correct Dhan strings.
    #[test]
    fn ab_order_status_as_str() {
        assert_eq!(OrderStatus::Transit.as_str(), "TRANSIT");
        assert_eq!(OrderStatus::Pending.as_str(), "PENDING");
        assert_eq!(OrderStatus::Confirmed.as_str(), "CONFIRMED");
        assert_eq!(OrderStatus::PartTraded.as_str(), "PART_TRADED");
        assert_eq!(OrderStatus::Traded.as_str(), "TRADED");
        assert_eq!(OrderStatus::Cancelled.as_str(), "CANCELLED");
        assert_eq!(OrderStatus::Rejected.as_str(), "REJECTED");
        assert_eq!(OrderStatus::Expired.as_str(), "EXPIRED");
        assert_eq!(OrderStatus::Closed.as_str(), "CLOSED");
        assert_eq!(OrderStatus::Triggered.as_str(), "TRIGGERED");
    }

    /// AB-ORD-03: OrderStatus Debug impl produces correct output.
    #[test]
    fn ab_order_status_debug() {
        assert_eq!(format!("{:?}", OrderStatus::Transit), "Transit");
        assert_eq!(format!("{:?}", OrderStatus::Pending), "Pending");
        assert_eq!(format!("{:?}", OrderStatus::Rejected), "Rejected");
        assert_eq!(format!("{:?}", OrderStatus::Cancelled), "Cancelled");
        assert_eq!(format!("{:?}", OrderStatus::PartTraded), "PartTraded");
        assert_eq!(format!("{:?}", OrderStatus::Traded), "Traded");
        assert_eq!(format!("{:?}", OrderStatus::Expired), "Expired");
        assert_eq!(format!("{:?}", OrderStatus::Closed), "Closed");
        assert_eq!(format!("{:?}", OrderStatus::Triggered), "Triggered");
    }

    /// AB-ORD-04: OrderStatus equality works for all variants.
    #[test]
    fn ab_order_status_equality() {
        assert_eq!(OrderStatus::Transit, OrderStatus::Transit);
        assert_ne!(OrderStatus::Transit, OrderStatus::Pending);
        assert_ne!(OrderStatus::Traded, OrderStatus::PartTraded);
    }
}

#[allow(
    clippy::unwrap_used,
    clippy::arithmetic_side_effects,
    clippy::assertions_on_constants
)] // APPROVED: S4 schema-drift guards
mod instrument_types_ab_tests {
    use dhan_live_trader_common::types::InstrumentType;

    /// AB-INST-01: All 4 instrument type variants exist.
    #[test]
    fn ab_all_instrument_types() {
        let _index = InstrumentType::Index;
        let _equity = InstrumentType::Equity;
        let _future = InstrumentType::Future;
        let _option = InstrumentType::Option;
    }

    /// AB-INST-02: InstrumentType as_str produces correct strings.
    #[test]
    fn ab_instrument_type_as_str() {
        assert_eq!(InstrumentType::Index.as_str(), "Index");
        assert_eq!(InstrumentType::Equity.as_str(), "Equity");
        assert_eq!(InstrumentType::Future.as_str(), "Future");
        assert_eq!(InstrumentType::Option.as_str(), "Option");
    }

    /// AB-INST-03: InstrumentType Debug impl.
    #[test]
    fn ab_instrument_type_debug() {
        assert_eq!(format!("{:?}", InstrumentType::Index), "Index");
        assert_eq!(format!("{:?}", InstrumentType::Equity), "Equity");
        assert_eq!(format!("{:?}", InstrumentType::Future), "Future");
        assert_eq!(format!("{:?}", InstrumentType::Option), "Option");
    }
}

#[allow(
    clippy::unwrap_used,
    clippy::arithmetic_side_effects,
    clippy::assertions_on_constants
)] // APPROVED: S4 schema-drift guards
mod feed_mode_ab_tests {
    use dhan_live_trader_common::types::FeedMode;

    /// AB-FEED-01: All feed modes exist.
    #[test]
    fn ab_all_feed_modes() {
        let _ticker = FeedMode::Ticker;
        let _quote = FeedMode::Quote;
        let _full = FeedMode::Full;
    }

    /// AB-FEED-02: FeedMode as_str produces correct strings.
    #[test]
    fn ab_feed_mode_as_str() {
        assert_eq!(FeedMode::Ticker.as_str(), "Ticker");
        assert_eq!(FeedMode::Quote.as_str(), "Quote");
        assert_eq!(FeedMode::Full.as_str(), "Full");
    }

    /// AB-FEED-03: FeedMode Display matches as_str.
    #[test]
    fn ab_feed_mode_display() {
        assert_eq!(format!("{}", FeedMode::Ticker), "Ticker");
        assert_eq!(format!("{}", FeedMode::Quote), "Quote");
        assert_eq!(format!("{}", FeedMode::Full), "Full");
    }
}

#[allow(
    clippy::unwrap_used,
    clippy::arithmetic_side_effects,
    clippy::assertions_on_constants
)] // APPROVED: S4 schema-drift guards
mod packet_structure_ab_tests {
    use dhan_live_trader_common::constants::*;

    /// AB-PKT-01: Full packet = header + LTP/LTQ/LTT/... + 5 depth levels.
    #[test]
    fn ab_full_packet_structure() {
        // 8 header + 54 fields + 100 depth (5 levels x 20 bytes) = 162
        assert_eq!(
            BINARY_HEADER_SIZE + 54 + MARKET_DEPTH_LEVELS * MARKET_DEPTH_LEVEL_SIZE,
            FULL_QUOTE_PACKET_SIZE,
            "Full packet structure: 8 + 54 + 5*20 = 162"
        );
    }

    /// AB-PKT-02: Twenty-depth packet = depth header + 20 levels.
    #[test]
    fn ab_twenty_depth_packet_structure() {
        assert_eq!(
            DEEP_DEPTH_HEADER_SIZE + TWENTY_DEPTH_LEVELS * DEEP_DEPTH_LEVEL_SIZE,
            TWENTY_DEPTH_PACKET_SIZE,
            "Twenty-depth: 12 + 20*16 = 332"
        );
    }

    /// AB-PKT-03: Ticker packet = header + LTP + LTT only.
    #[test]
    fn ab_ticker_minimal() {
        assert_eq!(
            TICKER_PACKET_SIZE,
            BINARY_HEADER_SIZE + 4 + 4, // f32 LTP + u32 LTT
            "Ticker: header(8) + LTP(4) + LTT(4) = 16"
        );
    }

    /// AB-PKT-04: OI packet = header + u32 OI.
    #[test]
    fn ab_oi_packet_minimal() {
        assert_eq!(
            OI_PACKET_SIZE,
            BINARY_HEADER_SIZE + 4, // u32 OI
            "OI: header(8) + OI(4) = 12"
        );
    }

    /// AB-PKT-05: PrevClose = header + f32 price + u32 OI.
    #[test]
    fn ab_prev_close_packet_structure() {
        assert_eq!(
            PREVIOUS_CLOSE_PACKET_SIZE,
            BINARY_HEADER_SIZE + 4 + 4, // f32 price + u32 OI
            "PrevClose: header(8) + price(4) + OI(4) = 16"
        );
    }

    /// AB-PKT-06: Deep depth header offsets are self-consistent.
    #[test]
    fn ab_deep_depth_header_layout() {
        assert_eq!(DEEP_DEPTH_HEADER_OFFSET_MSG_LENGTH, 0);
        assert_eq!(DEEP_DEPTH_HEADER_OFFSET_FEED_CODE, 2);
        assert_eq!(DEEP_DEPTH_HEADER_OFFSET_EXCHANGE_SEGMENT, 3);
        assert_eq!(DEEP_DEPTH_HEADER_OFFSET_SECURITY_ID, 4);
        assert_eq!(DEEP_DEPTH_HEADER_OFFSET_MSG_SEQUENCE, 8);
        // All offsets fit within 12-byte header
        assert!(DEEP_DEPTH_HEADER_OFFSET_MSG_SEQUENCE + 4 <= DEEP_DEPTH_HEADER_SIZE);
    }

    /// AB-PKT-07: Depth level field offsets are self-consistent.
    #[test]
    fn ab_depth_level_offsets() {
        assert_eq!(DEPTH_LEVEL_OFFSET_BID_QTY, 0);
        assert_eq!(DEPTH_LEVEL_OFFSET_ASK_QTY, 4);
        assert_eq!(DEPTH_LEVEL_OFFSET_BID_ORDERS, 8);
        assert_eq!(DEPTH_LEVEL_OFFSET_ASK_ORDERS, 10);
        assert_eq!(DEPTH_LEVEL_OFFSET_BID_PRICE, 12);
        assert_eq!(DEPTH_LEVEL_OFFSET_ASK_PRICE, 16);
        // All offsets + field size fit within level size
        assert!(DEPTH_LEVEL_OFFSET_ASK_PRICE + 4 <= MARKET_DEPTH_LEVEL_SIZE);
    }

    /// AB-PKT-08: Deep depth level offsets are self-consistent.
    #[test]
    fn ab_deep_depth_level_offsets() {
        assert_eq!(DEEP_DEPTH_LEVEL_OFFSET_PRICE, 0);
        assert_eq!(DEEP_DEPTH_LEVEL_OFFSET_QUANTITY, 8);
        assert_eq!(DEEP_DEPTH_LEVEL_OFFSET_ORDERS, 12);
        // All offsets + field size fit within deep depth level size
        assert!(DEEP_DEPTH_LEVEL_OFFSET_ORDERS + 4 <= DEEP_DEPTH_LEVEL_SIZE);
    }

    /// AB-PKT-09: Market depth packet size for 5-level (not Full mode).
    #[test]
    fn ab_market_depth_packet() {
        assert_eq!(MARKET_DEPTH_PACKET_SIZE, 112);
    }

    /// AB-PKT-10: Max instruments per 20-depth connection.
    #[test]
    fn ab_twenty_depth_instrument_limit() {
        assert_eq!(
            MAX_INSTRUMENTS_PER_TWENTY_DEPTH_CONNECTION, 50,
            "20-level depth: max 50 instruments per connection"
        );
    }

    /// AB-PKT-11: Max instruments per 200-depth connection.
    #[test]
    fn ab_two_hundred_depth_instrument_limit() {
        assert_eq!(
            MAX_INSTRUMENTS_PER_TWO_HUNDRED_DEPTH_CONNECTION, 1,
            "200-level depth: exactly 1 instrument per connection"
        );
    }
}
