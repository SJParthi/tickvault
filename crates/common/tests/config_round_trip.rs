//! Integration tests for config types — TOML deserialization and TradingCalendar validation.
//!
//! Tests the public API surface of the common crate through integration tests:
//! - TOML string → ApplicationConfig deserialization (production code path)
//! - TradingConfig → TradingCalendar construction and query
//! - Default value correctness for optional config sections

use chrono::NaiveDate;
use dhan_live_trader_common::config::{ApplicationConfig, NseHolidayEntry, TradingConfig};
use dhan_live_trader_common::trading_calendar::TradingCalendar;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Minimal valid TOML that deserializes into a complete `ApplicationConfig`.
/// Mirrors `config/base.toml` structure with reduced holiday list (10 = minimum).
const VALID_CONFIG_TOML: &str = r#"
[trading]
market_open_time = "09:15:00"
market_close_time = "15:30:00"
order_cutoff_time = "15:29:00"
data_collection_start = "09:00:00"
data_collection_end = "15:30:00"
timezone = "Asia/Kolkata"
max_orders_per_second = 10

[[trading.nse_holidays]]
date = "2026-01-26"
name = "Republic Day"

[[trading.nse_holidays]]
date = "2026-03-03"
name = "Holi"

[[trading.nse_holidays]]
date = "2026-03-26"
name = "Shri Ram Navami"

[[trading.nse_holidays]]
date = "2026-03-31"
name = "Shri Mahavir Jayanti"

[[trading.nse_holidays]]
date = "2026-04-03"
name = "Good Friday"

[[trading.nse_holidays]]
date = "2026-04-14"
name = "Dr. Baba Saheb Ambedkar Jayanti"

[[trading.nse_holidays]]
date = "2026-05-01"
name = "Maharashtra Day"

[[trading.nse_holidays]]
date = "2026-05-28"
name = "Bakri Eid"

[[trading.nse_holidays]]
date = "2026-06-26"
name = "Muharram"

[[trading.nse_holidays]]
date = "2026-09-14"
name = "Ganesh Chaturthi"

[[trading.muhurat_trading_dates]]
date = "2026-11-08"
name = "Diwali 2026"

[dhan]
websocket_url = "wss://api-feed.dhan.co"
order_update_websocket_url = "wss://api-order-update.dhan.co"
rest_api_base_url = "https://api.dhan.co/v2"
auth_base_url = "https://auth.dhan.co"
instrument_csv_url = "https://images.dhan.co/api-data/api-scrip-master-detailed.csv"
instrument_csv_fallback_url = "https://images.dhan.co/api-data/api-scrip-master.csv"
max_instruments_per_connection = 5000
max_websocket_connections = 5

[questdb]
host = "dlt-questdb"
http_port = 9000
pg_port = 8812
ilp_port = 9009

[valkey]
host = "dlt-valkey"
port = 6379
max_connections = 16

[prometheus]
host = "dlt-prometheus"
port = 9090

[websocket]
ping_interval_secs = 10
pong_timeout_secs = 10
max_consecutive_pong_failures = 2
reconnect_initial_delay_ms = 500
reconnect_max_delay_ms = 30000
reconnect_max_attempts = 10
subscription_batch_size = 100
connection_stagger_ms = 10000

[network]
request_timeout_ms = 10000
websocket_connect_timeout_ms = 10000
retry_initial_delay_ms = 100
retry_max_delay_ms = 30000
retry_max_attempts = 5

[token]
refresh_before_expiry_hours = 1
token_validity_hours = 24

[risk]
max_daily_loss_percent = 2.0
max_position_size_lots = 50

[logging]
level = "info"
format = "json"

[instrument]
daily_download_time = "08:15:00"
csv_cache_directory = "/app/data/instrument-cache"
csv_cache_filename = "api-scrip-master-detailed.csv"
csv_download_timeout_secs = 120
build_window_start = "09:00:00"
build_window_end = "15:30:00"

[api]
host = "0.0.0.0"
port = 3001
"#;

/// Helper to create an NseHolidayEntry.
fn holiday(date: &str, name: &str) -> NseHolidayEntry {
    NseHolidayEntry {
        date: date.to_string(),
        name: name.to_string(),
    }
}

/// Builds a TradingConfig with 10 holidays (minimum required).
fn make_trading_config_with_holidays() -> TradingConfig {
    TradingConfig {
        market_open_time: "09:15:00".to_string(),
        market_close_time: "15:30:00".to_string(),
        order_cutoff_time: "15:29:00".to_string(),
        data_collection_start: "09:00:00".to_string(),
        data_collection_end: "15:30:00".to_string(),
        timezone: "Asia/Kolkata".to_string(),
        max_orders_per_second: 10,
        nse_holidays: vec![
            holiday("2026-01-26", "Republic Day"),
            holiday("2026-03-03", "Holi"),
            holiday("2026-03-26", "Shri Ram Navami"),
            holiday("2026-03-31", "Shri Mahavir Jayanti"),
            holiday("2026-04-03", "Good Friday"),
            holiday("2026-04-14", "Dr. Baba Saheb Ambedkar Jayanti"),
            holiday("2026-05-01", "Maharashtra Day"),
            holiday("2026-05-28", "Bakri Eid"),
            holiday("2026-06-26", "Muharram"),
            holiday("2026-09-14", "Ganesh Chaturthi"),
        ],
        muhurat_trading_dates: vec![],
    }
}

// ---------------------------------------------------------------------------
// TOML deserialization tests
// ---------------------------------------------------------------------------

#[test]
fn test_config_toml_deserializes_all_sections() {
    let config: ApplicationConfig =
        toml::from_str(VALID_CONFIG_TOML).expect("valid TOML must deserialize");

    // Spot-check key fields across multiple sections.
    assert_eq!(config.trading.timezone, "Asia/Kolkata");
    assert_eq!(config.trading.max_orders_per_second, 10);
    assert_eq!(config.trading.nse_holidays.len(), 10);
    assert_eq!(config.dhan.rest_api_base_url, "https://api.dhan.co/v2");
    assert_eq!(config.questdb.http_port, 9000);
    assert_eq!(config.valkey.port, 6379);
    assert_eq!(config.websocket.subscription_batch_size, 100);
    assert_eq!(config.network.retry_max_attempts, 5);
    assert_eq!(config.token.token_validity_hours, 24);
    assert_eq!(config.api.port, 3001);
}

#[test]
fn test_config_toml_round_trip_validates() {
    let config: ApplicationConfig =
        toml::from_str(VALID_CONFIG_TOML).expect("valid TOML must deserialize");

    // The deserialized config must pass full validation (production code path).
    config
        .validate()
        .expect("deserialized config must validate");
}

#[test]
fn test_config_toml_holiday_entries_have_correct_shape() {
    let config: ApplicationConfig =
        toml::from_str(VALID_CONFIG_TOML).expect("valid TOML must deserialize");

    let first = &config.trading.nse_holidays[0];
    assert_eq!(first.date, "2026-01-26");
    assert_eq!(first.name, "Republic Day");

    let muhurat = &config.trading.muhurat_trading_dates[0];
    assert_eq!(muhurat.date, "2026-11-08");
    assert_eq!(muhurat.name, "Diwali 2026");
}

// ---------------------------------------------------------------------------
// TradingCalendar from config
// ---------------------------------------------------------------------------

#[test]
fn test_trading_calendar_from_config_valid() {
    let config = make_trading_config_with_holidays();
    let calendar = TradingCalendar::from_config(&config).expect("valid config");

    assert_eq!(calendar.holiday_count(), 10);
    assert_eq!(calendar.muhurat_count(), 0);
}

#[test]
fn test_trading_calendar_holiday_detection() {
    let config = make_trading_config_with_holidays();
    let calendar = TradingCalendar::from_config(&config).expect("valid config");

    // Republic Day 2026 (Monday) — holiday.
    let republic_day = NaiveDate::from_ymd_opt(2026, 1, 26).expect("valid date");
    assert!(!calendar.is_trading_day(republic_day));
    assert!(calendar.is_holiday(republic_day));

    // Day after Republic Day (Tuesday) — regular trading day.
    let jan_27 = NaiveDate::from_ymd_opt(2026, 1, 27).expect("valid date");
    assert!(calendar.is_trading_day(jan_27));
    assert!(!calendar.is_holiday(jan_27));
}

#[test]
fn test_trading_calendar_weekend_detection() {
    let config = make_trading_config_with_holidays();
    let calendar = TradingCalendar::from_config(&config).expect("valid config");

    // Saturday — not a trading day but not a listed holiday either.
    let saturday = NaiveDate::from_ymd_opt(2026, 3, 7).expect("valid date");
    assert!(!calendar.is_trading_day(saturday));
    assert!(!calendar.is_holiday(saturday));

    // Sunday — same.
    let sunday = NaiveDate::from_ymd_opt(2026, 3, 8).expect("valid date");
    assert!(!calendar.is_trading_day(sunday));
}

#[test]
fn test_trading_calendar_next_trading_day_skips_weekend() {
    let config = make_trading_config_with_holidays();
    let calendar = TradingCalendar::from_config(&config).expect("valid config");

    let saturday = NaiveDate::from_ymd_opt(2026, 3, 7).expect("valid date");
    let monday = NaiveDate::from_ymd_opt(2026, 3, 9).expect("valid date");
    assert_eq!(calendar.next_trading_day(saturday), monday);
}

#[test]
fn test_trading_calendar_next_trading_day_skips_holiday() {
    let config = make_trading_config_with_holidays();
    let calendar = TradingCalendar::from_config(&config).expect("valid config");

    // Republic Day 2026-01-26 (Mon) is a holiday, next trading day is Tue 2026-01-27.
    let republic_day = NaiveDate::from_ymd_opt(2026, 1, 26).expect("valid date");
    let jan_27 = NaiveDate::from_ymd_opt(2026, 1, 27).expect("valid date");
    assert_eq!(calendar.next_trading_day(republic_day), jan_27);
}

#[test]
fn test_trading_calendar_all_entries_sorted_by_date() {
    let mut config = make_trading_config_with_holidays();
    // Add a Muhurat date that falls on a weekend.
    config
        .muhurat_trading_dates
        .push(holiday("2026-11-08", "Diwali 2026"));

    let calendar = TradingCalendar::from_config(&config).expect("valid config");
    let entries = calendar.all_entries();

    // Entries must be sorted ascending by date.
    for window in entries.windows(2) {
        assert!(
            window[0].date <= window[1].date,
            "entries not sorted: {} > {}",
            window[0].date,
            window[1].date
        );
    }

    // Must contain both holidays and Muhurat entries.
    let muhurat_count = entries.iter().filter(|e| e.is_muhurat).count();
    assert_eq!(muhurat_count, 1);
    let holiday_count = entries.iter().filter(|e| !e.is_muhurat).count();
    assert_eq!(holiday_count, 10);
}

#[test]
fn test_trading_calendar_muhurat_session() {
    let mut config = make_trading_config_with_holidays();
    config
        .muhurat_trading_dates
        .push(holiday("2026-11-08", "Diwali 2026"));

    let calendar = TradingCalendar::from_config(&config).expect("valid config");
    let diwali = NaiveDate::from_ymd_opt(2026, 11, 8).expect("valid date");

    // Muhurat day: not a regular trading day (it's Sunday), but has a Muhurat session.
    assert!(!calendar.is_trading_day(diwali));
    assert!(calendar.is_muhurat_trading_day(diwali));
}

// ---------------------------------------------------------------------------
// Default value tests
// ---------------------------------------------------------------------------

#[test]
fn test_config_default_values() {
    // Optional sections with #[serde(default)] should get sensible defaults.
    let config: ApplicationConfig =
        toml::from_str(VALID_CONFIG_TOML).expect("valid TOML must deserialize");

    // SubscriptionConfig defaults.
    assert_eq!(config.subscription.feed_mode, "Ticker");
    assert!(config.subscription.subscribe_index_derivatives);
    assert_eq!(config.subscription.stock_atm_strikes_above, 10);

    // StrategyConfig defaults.
    assert_eq!(config.strategy.config_path, "config/strategies.toml");
    assert!((config.strategy.capital - 1_000_000.0).abs() < f64::EPSILON);
    // dry_run defaults to true (safe by default).
    assert!(config.strategy.dry_run);

    // HistoricalDataConfig defaults.
    assert!(config.historical.enabled);
    assert_eq!(config.historical.lookback_days, 90);

    // NotificationConfig defaults.
    assert_eq!(
        config.notification.telegram_api_base_url,
        "https://api.telegram.org"
    );

    // ObservabilityConfig defaults.
    assert_eq!(config.observability.metrics_port, 9091);
    assert!(config.observability.metrics_enabled);
}

#[test]
fn test_strategy_dry_run_defaults_true() {
    // Critical safety check: dry_run must default to true.
    let strategy = dhan_live_trader_common::config::StrategyConfig::default();
    assert!(strategy.dry_run, "dry_run must default to true for safety");
}

// ---------------------------------------------------------------------------
// Validation failure tests (via TOML deserialization path)
// ---------------------------------------------------------------------------

#[test]
fn test_config_validation_rejects_wrong_timezone() {
    let toml_with_utc = VALID_CONFIG_TOML.replace("Asia/Kolkata", "UTC");
    let config: ApplicationConfig =
        toml::from_str(&toml_with_utc).expect("TOML must parse even with wrong timezone");
    let err = config
        .validate()
        .expect_err("UTC timezone must fail validation");
    assert!(err.to_string().contains("Asia/Kolkata"));
}

#[test]
fn test_config_validation_rejects_sebi_order_limit_exceeded() {
    let toml_over_sebi =
        VALID_CONFIG_TOML.replace("max_orders_per_second = 10", "max_orders_per_second = 11");
    let config: ApplicationConfig = toml::from_str(&toml_over_sebi).expect("TOML must parse");
    let err = config
        .validate()
        .expect_err("orders > SEBI limit must fail");
    assert!(err.to_string().contains("SEBI"));
}
