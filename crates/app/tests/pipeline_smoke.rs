//! Smoke integration tests for the app crate.
//!
//! The app crate is a binary-only crate (no lib.rs), so these tests
//! verify that key types from dependencies compile and behave as expected.

use dhan_live_trader_common::config::NseHolidayEntry;
use dhan_live_trader_common::constants::{
    DEDUP_RING_BUFFER_POWER, IST_UTC_OFFSET_SECONDS, MINIMUM_VALID_EXCHANGE_TIMESTAMP,
};
use dhan_live_trader_common::trading_calendar::TradingCalendar;

// ---------------------------------------------------------------------------
// Constants smoke tests
// ---------------------------------------------------------------------------

#[test]
fn test_ist_utc_offset_is_19800_seconds() {
    // IST = UTC+5:30 = 5*3600 + 30*60 = 19800
    assert_eq!(IST_UTC_OFFSET_SECONDS, 19800);
}

#[test]
fn test_dedup_ring_buffer_power_in_valid_range() {
    // DEDUP_RING_BUFFER_POWER must be in [8, 24] per TickDedupRing::new() contract.
    assert!(
        (8..=24).contains(&DEDUP_RING_BUFFER_POWER),
        "DEDUP_RING_BUFFER_POWER={DEDUP_RING_BUFFER_POWER} out of valid range [8, 24]"
    );
}

#[test]
fn test_minimum_valid_exchange_timestamp_positive() {
    // Must be > 0 (some epoch after Unix epoch start)
    const { assert!(MINIMUM_VALID_EXCHANGE_TIMESTAMP > 0) };
    // Must be before year 2040 (~2208988800)
    const { assert!(MINIMUM_VALID_EXCHANGE_TIMESTAMP < 2_208_988_800) };
}

// ---------------------------------------------------------------------------
// TradingCalendar smoke tests (with standard 2026 holidays)
// ---------------------------------------------------------------------------

fn standard_2026_holidays() -> Vec<NseHolidayEntry> {
    vec![
        NseHolidayEntry {
            date: "2026-01-26".to_string(),
            name: "Republic Day".to_string(),
        },
        NseHolidayEntry {
            date: "2026-03-03".to_string(),
            name: "Holi".to_string(),
        },
        NseHolidayEntry {
            date: "2026-03-26".to_string(),
            name: "Shri Ram Navami".to_string(),
        },
        NseHolidayEntry {
            date: "2026-03-31".to_string(),
            name: "Shri Mahavir Jayanti".to_string(),
        },
        NseHolidayEntry {
            date: "2026-04-03".to_string(),
            name: "Good Friday".to_string(),
        },
        NseHolidayEntry {
            date: "2026-04-14".to_string(),
            name: "Ambedkar Jayanti".to_string(),
        },
        NseHolidayEntry {
            date: "2026-05-01".to_string(),
            name: "Maharashtra Day".to_string(),
        },
        NseHolidayEntry {
            date: "2026-05-28".to_string(),
            name: "Bakri Eid".to_string(),
        },
        NseHolidayEntry {
            date: "2026-06-26".to_string(),
            name: "Muharram".to_string(),
        },
        NseHolidayEntry {
            date: "2026-09-14".to_string(),
            name: "Ganesh Chaturthi".to_string(),
        },
        NseHolidayEntry {
            date: "2026-10-02".to_string(),
            name: "Gandhi Jayanti".to_string(),
        },
        NseHolidayEntry {
            date: "2026-10-20".to_string(),
            name: "Dussehra".to_string(),
        },
        NseHolidayEntry {
            date: "2026-11-10".to_string(),
            name: "Diwali Balipratipada".to_string(),
        },
        NseHolidayEntry {
            date: "2026-11-24".to_string(),
            name: "Guru Nanak Gurpurb".to_string(),
        },
        NseHolidayEntry {
            date: "2026-12-25".to_string(),
            name: "Christmas".to_string(),
        },
    ]
}

fn test_trading_config() -> dhan_live_trader_common::config::TradingConfig {
    dhan_live_trader_common::config::TradingConfig {
        market_open_time: "09:00:00".to_string(),
        market_close_time: "15:30:00".to_string(),
        order_cutoff_time: "15:29:00".to_string(),
        data_collection_start: "09:00:00".to_string(),
        data_collection_end: "15:30:00".to_string(),
        timezone: "Asia/Kolkata".to_string(),
        max_orders_per_second: 10,
        nse_holidays: standard_2026_holidays(),
        muhurat_trading_dates: vec![NseHolidayEntry {
            date: "2026-11-08".to_string(),
            name: "Diwali 2026".to_string(),
        }],
    }
}

#[test]
fn test_trading_calendar_construction_succeeds() {
    let config = test_trading_config();
    let calendar = TradingCalendar::from_config(&config);
    assert!(
        calendar.is_ok(),
        "TradingCalendar construction failed: {:?}",
        calendar.err()
    );
}

#[test]
fn test_trading_calendar_weekday_is_trading_day() {
    let config = test_trading_config();
    let calendar = TradingCalendar::from_config(&config).expect("valid config"); // APPROVED: test-only
    // Monday 2026-01-05 is a weekday with no holiday
    let monday = chrono::NaiveDate::from_ymd_opt(2026, 1, 5).expect("valid date"); // APPROVED: test-only
    assert!(calendar.is_trading_day(monday));
}

#[test]
fn test_trading_calendar_weekend_is_not_trading_day() {
    let config = test_trading_config();
    let calendar = TradingCalendar::from_config(&config).expect("valid config"); // APPROVED: test-only
    // Saturday 2026-01-03
    let saturday = chrono::NaiveDate::from_ymd_opt(2026, 1, 3).expect("valid date"); // APPROVED: test-only
    assert!(!calendar.is_trading_day(saturday));
}

#[test]
fn test_trading_calendar_holiday_is_not_trading_day() {
    let config = test_trading_config();
    let calendar = TradingCalendar::from_config(&config).expect("valid config"); // APPROVED: test-only
    // Republic Day 2026-01-26 (Monday)
    let republic_day = chrono::NaiveDate::from_ymd_opt(2026, 1, 26).expect("valid date"); // APPROVED: test-only
    assert!(!calendar.is_trading_day(republic_day));
}

// ---------------------------------------------------------------------------
// Smoke: config/base.toml can be parsed into ApplicationConfig
// ---------------------------------------------------------------------------

#[test]
fn test_smoke_config_loads_from_base_toml() {
    use dhan_live_trader_common::config::ApplicationConfig;
    use figment::Figment;
    use figment::providers::{Format, Toml};

    // The base.toml file must exist and be parseable.
    // Integration tests run with cwd = workspace root (Cargo convention).
    // Fall back to relative-from-crate path if workspace root path fails.
    let config_path = if std::path::Path::new("config/base.toml").exists() {
        "config/base.toml"
    } else if std::path::Path::new("../../config/base.toml").exists() {
        "../../config/base.toml"
    } else {
        panic!("config/base.toml not found from workspace root or crate directory")
    };

    let result: Result<ApplicationConfig, _> =
        Figment::new().merge(Toml::file(config_path)).extract();

    assert!(
        result.is_ok(),
        "config/base.toml failed to parse: {:?}",
        result.err()
    );

    let config = result.expect("already checked"); // APPROVED: test-only
    // Sanity checks on parsed config.
    assert!(
        !config.trading.market_open_time.is_empty(),
        "market_open_time must not be empty"
    );
    assert!(
        !config.trading.market_close_time.is_empty(),
        "market_close_time must not be empty"
    );
    assert!(
        config.trading.max_orders_per_second > 0,
        "max_orders_per_second must be positive"
    );
    assert!(
        config.trading.max_orders_per_second <= 10,
        "max_orders_per_second must not exceed SEBI limit of 10"
    );
    assert!(
        !config.trading.nse_holidays.is_empty(),
        "nse_holidays must not be empty"
    );
}

// ---------------------------------------------------------------------------
// Smoke: all critical constants are in valid ranges
// ---------------------------------------------------------------------------

#[test]
fn test_smoke_all_constants_valid() {
    use dhan_live_trader_common::constants::*;

    // --- Packet sizes: positive and reasonable ---
    const {
        assert!(TICKER_PACKET_SIZE > 0 && TICKER_PACKET_SIZE <= 256);
    }
    const {
        assert!(QUOTE_PACKET_SIZE > 0 && QUOTE_PACKET_SIZE <= 256);
    }
    const {
        assert!(FULL_QUOTE_PACKET_SIZE > 0 && FULL_QUOTE_PACKET_SIZE <= 1024);
    }
    const {
        assert!(DISCONNECT_PACKET_SIZE > 0 && DISCONNECT_PACKET_SIZE <= 256);
    }
    const {
        assert!(OI_PACKET_SIZE > 0 && OI_PACKET_SIZE <= 256);
    }
    const {
        assert!(PREVIOUS_CLOSE_PACKET_SIZE > 0 && PREVIOUS_CLOSE_PACKET_SIZE <= 256);
    }
    const {
        assert!(MARKET_STATUS_PACKET_SIZE > 0 && MARKET_STATUS_PACKET_SIZE <= 256);
    }
    const {
        assert!(MARKET_DEPTH_PACKET_SIZE > 0 && MARKET_DEPTH_PACKET_SIZE <= 256);
    }

    // --- Connection limits: positive and within Dhan spec ---
    const {
        assert!(MAX_WEBSOCKET_CONNECTIONS >= 1 && MAX_WEBSOCKET_CONNECTIONS <= 10);
    }
    const {
        assert!(MAX_INSTRUMENTS_PER_WEBSOCKET_CONNECTION >= 100);
    }
    const {
        assert!(MAX_INSTRUMENTS_PER_WEBSOCKET_CONNECTION <= 10_000);
    }
    const {
        assert!(SUBSCRIPTION_BATCH_SIZE >= 1 && SUBSCRIPTION_BATCH_SIZE <= 100);
    }

    // --- Ring buffer capacities: must be powers of 2 ---
    assert!(TICK_RING_BUFFER_CAPACITY.is_power_of_two());
    assert!(ORDER_EVENT_RING_BUFFER_CAPACITY.is_power_of_two());
    assert!(INDICATOR_RING_BUFFER_CAPACITY.is_power_of_two());

    // --- Tick persist window: valid IST times ---
    const {
        assert!(TICK_PERSIST_START_SECS_OF_DAY_IST < TICK_PERSIST_END_SECS_OF_DAY_IST);
    }
    const {
        assert!(TICK_PERSIST_END_SECS_OF_DAY_IST < SECONDS_PER_DAY);
    }

    // --- SEBI compliance ---
    assert_eq!(SEBI_MAX_ORDERS_PER_SECOND, 10);
    const {
        assert!(SEBI_AUDIT_RETENTION_YEARS >= 5);
    }

    // --- IST offset consistency ---
    assert_eq!(IST_UTC_OFFSET_SECONDS, 19_800);
    assert_eq!(IST_UTC_OFFSET_SECONDS_I64, 19_800);
    assert_eq!(
        IST_UTC_OFFSET_NANOS,
        IST_UTC_OFFSET_SECONDS_I64 * 1_000_000_000
    );

    // --- Deep depth header differs from standard ---
    assert_ne!(DEEP_DEPTH_HEADER_SIZE, BINARY_HEADER_SIZE);
    const {
        assert!(DEEP_DEPTH_HEADER_SIZE > BINARY_HEADER_SIZE);
    }

    // --- TOTP config ---
    assert_eq!(TOTP_DIGITS, 6);
    assert_eq!(TOTP_PERIOD_SECS, 30);

    // --- OMS circuit breaker ---
    const {
        assert!(OMS_CIRCUIT_BREAKER_FAILURE_THRESHOLD >= 1);
    }
    const {
        assert!(OMS_CIRCUIT_BREAKER_RESET_SECS >= 1);
    }

    // --- Application identity ---
    assert!(!APPLICATION_NAME.is_empty());
    assert!(!APPLICATION_VERSION.is_empty());
}

// ---------------------------------------------------------------------------
// Smoke: all error enum variants have Display impl
// ---------------------------------------------------------------------------

#[test]
fn test_smoke_error_types_have_display() {
    use dhan_live_trader_common::error::ApplicationError;

    // Construct one instance of each variant and verify Display produces non-empty output.
    let variants: Vec<Box<dyn std::fmt::Display>> = vec![
        Box::new(ApplicationError::Configuration("test".to_string())),
        Box::new(ApplicationError::SecretRetrieval {
            path: "/dlt/dev/test".to_string(),
            source: anyhow::anyhow!("test error"),
        }),
        Box::new(ApplicationError::MarketHourViolation("test".to_string())),
        Box::new(ApplicationError::InfrastructureUnavailable {
            service: "test".to_string(),
            endpoint: "localhost".to_string(),
        }),
        Box::new(ApplicationError::InstrumentDownloadFailed {
            reason: "test".to_string(),
        }),
        Box::new(ApplicationError::InstrumentParseFailed {
            row: 1,
            reason: "test".to_string(),
        }),
        Box::new(ApplicationError::UniverseValidationFailed {
            check: "test".to_string(),
        }),
        Box::new(ApplicationError::CsvColumnMissing {
            column: "TEST".to_string(),
        }),
        Box::new(ApplicationError::QuestDbWriteFailed {
            table: "test".to_string(),
            source: anyhow::anyhow!("test"),
        }),
        Box::new(ApplicationError::TotpGenerationFailed {
            reason: "test".to_string(),
        }),
        Box::new(ApplicationError::AuthenticationFailed {
            reason: "test".to_string(),
        }),
        Box::new(ApplicationError::TokenRenewalFailed {
            attempts: 3,
            reason: "test".to_string(),
        }),
        Box::new(ApplicationError::AuthCircuitBreakerTripped { failures: 3 }),
        Box::new(ApplicationError::NotificationSendFailed {
            reason: "test".to_string(),
        }),
        Box::new(ApplicationError::IpVerificationFailed {
            reason: "test".to_string(),
        }),
    ];

    for (idx, variant) in variants.iter().enumerate() {
        let display = format!("{variant}");
        assert!(
            !display.is_empty(),
            "ApplicationError variant {idx} has empty Display output"
        );
        // Every Display output should contain the word we passed in (case-insensitive).
        let display_lower = display.to_lowercase();
        assert!(
            display_lower.contains("test") || display_lower.contains("3"),
            "ApplicationError variant {idx} Display does not contain expected content: {display}"
        );
    }
}
