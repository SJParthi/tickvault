//! Configuration structs deserialized from `config/base.toml`.
//!
//! Every runtime value that might differ between environments lives here.
//! Secrets are NEVER in config — they come from AWS SSM Parameter Store.

use anyhow::{Result, bail};
use chrono::NaiveTime;
use serde::Deserialize;

use crate::constants::SEBI_MAX_ORDERS_PER_SECOND;
use crate::trading_calendar::TradingCalendar;

/// Root application configuration.
#[derive(Debug, Deserialize)]
pub struct ApplicationConfig {
    pub trading: TradingConfig,
    pub dhan: DhanConfig,
    pub websocket: WebSocketConfig,
    pub questdb: QuestDbConfig,
    pub valkey: ValkeyConfig,
    pub prometheus: PrometheusConfig,
    pub network: NetworkConfig,
    pub token: TokenConfig,
    pub risk: RiskConfig,
    pub logging: LoggingConfig,
    pub instrument: InstrumentConfig,
    pub api: ApiConfig,
    #[serde(default)]
    pub subscription: SubscriptionConfig,
    #[serde(default)]
    pub notification: NotificationConfig,
    #[serde(default)]
    pub observability: ObservabilityConfig,
    #[serde(default)]
    pub historical: HistoricalDataConfig,
    #[serde(default)]
    pub strategy: StrategyConfig,
}

/// Strategy and paper-trading configuration.
#[derive(Debug, Deserialize)]
pub struct StrategyConfig {
    /// Path to the strategy TOML config file (relative to working directory).
    #[serde(default = "default_strategy_config_path")]
    pub config_path: String,
    /// Trading capital in rupees (for risk engine daily loss calculation).
    #[serde(default = "default_capital")]
    pub capital: f64,
    /// Dry-run mode: when true, NO real orders are placed. All orders are simulated.
    /// DEFAULT: true. This is a developer-only tool.
    #[serde(default = "default_dry_run")]
    pub dry_run: bool,
}

impl Default for StrategyConfig {
    fn default() -> Self {
        Self {
            config_path: default_strategy_config_path(),
            capital: default_capital(),
            dry_run: default_dry_run(),
        }
    }
}

fn default_strategy_config_path() -> String {
    "config/strategies.toml".to_string()
}

const fn default_capital() -> f64 {
    1_000_000.0
}

const fn default_dry_run() -> bool {
    true
}

/// Trading session timing configuration.
#[derive(Debug, Deserialize)]
pub struct TradingConfig {
    /// Market open time in IST (e.g., "09:00:00").
    pub market_open_time: String,
    /// Market close time in IST (e.g., "15:30:00").
    pub market_close_time: String,
    /// Order submission cutoff in IST (e.g., "15:29:00").
    pub order_cutoff_time: String,
    /// Data collection start time in IST (e.g., "09:00:00").
    pub data_collection_start: String,
    /// Data collection end time in IST (e.g., "16:00:00").
    pub data_collection_end: String,
    /// Timezone identifier (always "Asia/Kolkata").
    pub timezone: String,
    /// Maximum orders per second (SEBI limit).
    pub max_orders_per_second: u32,
    /// NSE trading holidays with names for display.
    /// Source: official NSE circular (update annually).
    #[serde(default)]
    pub nse_holidays: Vec<NseHolidayEntry>,
    /// Muhurat Trading dates — special sessions on otherwise closed days.
    #[serde(default)]
    pub muhurat_trading_dates: Vec<NseHolidayEntry>,
}

/// A single NSE holiday or Muhurat trading date with display name.
#[derive(Debug, Clone, Deserialize)]
pub struct NseHolidayEntry {
    /// Date in YYYY-MM-DD format (IST).
    pub date: String,
    /// Human-readable holiday name for display.
    pub name: String,
}

/// Dhan API and WebSocket connection configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct DhanConfig {
    /// WebSocket V2 binary feed URL.
    pub websocket_url: String,
    /// Order update WebSocket URL (JSON-based, separate from binary feed).
    pub order_update_websocket_url: String,
    /// REST API base URL (for trading, data, renewal).
    pub rest_api_base_url: String,
    /// Auth base URL (for token generation — separate from REST API).
    /// Dhan uses `https://auth.dhan.co` for authentication endpoints.
    pub auth_base_url: String,
    /// Primary instrument CSV download URL.
    pub instrument_csv_url: String,
    /// Fallback instrument CSV download URL.
    pub instrument_csv_fallback_url: String,
    /// Maximum instruments per WebSocket connection.
    pub max_instruments_per_connection: usize,
    /// Maximum concurrent WebSocket connections.
    pub max_websocket_connections: usize,
}

/// WebSocket keep-alive and reconnection configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct WebSocketConfig {
    /// Expected server ping interval in seconds (Dhan server pings every 10s).
    pub ping_interval_secs: u64,
    /// Server disconnects after this many seconds with no pong.
    pub pong_timeout_secs: u64,
    /// Reserved for future use (server ping monitoring).
    pub max_consecutive_pong_failures: u32,
    /// Initial reconnection delay in milliseconds.
    pub reconnect_initial_delay_ms: u64,
    /// Maximum reconnection delay in milliseconds.
    pub reconnect_max_delay_ms: u64,
    /// Maximum reconnection attempts before giving up.
    pub reconnect_max_attempts: u32,
    /// Maximum instruments per subscription message (Dhan limit: 100).
    pub subscription_batch_size: usize,
    /// Delay in milliseconds between spawning successive WebSocket connections.
    /// Prevents all connections from hitting Dhan's server simultaneously at startup.
    /// 0 = no stagger (all spawn immediately). Only affects initial startup, not reconnects.
    pub connection_stagger_ms: u64,
}

/// QuestDB connection configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct QuestDbConfig {
    /// QuestDB Docker hostname.
    pub host: String,
    /// HTTP API port (web console + REST).
    pub http_port: u16,
    /// PostgreSQL wire protocol port.
    pub pg_port: u16,
    /// InfluxDB Line Protocol port (high-speed ingestion).
    pub ilp_port: u16,
}

/// Valkey (cache) connection configuration.
#[derive(Debug, Deserialize)]
pub struct ValkeyConfig {
    /// Valkey Docker hostname.
    pub host: String,
    /// Valkey port.
    pub port: u16,
    /// Maximum connections in the connection pool.
    pub max_connections: u32,
}

/// Prometheus metrics endpoint configuration.
#[derive(Debug, Deserialize)]
pub struct PrometheusConfig {
    /// Prometheus Docker hostname.
    pub host: String,
    /// Prometheus port.
    pub port: u16,
}

/// Network timeout and retry configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct NetworkConfig {
    /// HTTP request timeout in milliseconds.
    pub request_timeout_ms: u64,
    /// WebSocket connection timeout in milliseconds.
    pub websocket_connect_timeout_ms: u64,
    /// Initial retry delay in milliseconds (exponential backoff).
    pub retry_initial_delay_ms: u64,
    /// Maximum retry delay in milliseconds.
    pub retry_max_delay_ms: u64,
    /// Maximum number of retry attempts.
    pub retry_max_attempts: u32,
}

/// JWT token lifecycle configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct TokenConfig {
    /// Hours before expiry to trigger token refresh.
    pub refresh_before_expiry_hours: u64,
    /// Token validity duration in hours.
    pub token_validity_hours: u64,
}

/// Risk management configuration.
#[derive(Debug, Deserialize)]
pub struct RiskConfig {
    /// Maximum allowed daily loss as percentage of capital.
    pub max_daily_loss_percent: f64,
    /// Maximum position size in lots.
    pub max_position_size_lots: u32,
}

/// Logging configuration.
#[derive(Debug, Deserialize)]
pub struct LoggingConfig {
    /// Log level filter (trace, debug, info, warn, error).
    pub level: String,
    /// Log output format (json, pretty).
    pub format: String,
}

/// API server configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct ApiConfig {
    /// Bind address for the HTTP server.
    pub host: String,
    /// HTTP server port.
    pub port: u16,
}

/// Instrument CSV download and universe build configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct InstrumentConfig {
    /// Time of day (IST) to download fresh instrument CSV. Format: "HH:MM:SS".
    pub daily_download_time: String,
    /// Directory path for caching the last successful CSV download.
    pub csv_cache_directory: String,
    /// Cached CSV filename.
    pub csv_cache_filename: String,
    /// Download timeout in seconds (overrides network timeout for this large file).
    pub csv_download_timeout_secs: u64,
    /// Start of instrument build window (IST). Format: "HH:MM:SS".
    pub build_window_start: String,
    /// End of instrument build window (IST). Format: "HH:MM:SS".
    pub build_window_end: String,
}

/// Notification (Telegram alert) configuration.
///
/// Secrets (bot token, chat ID) come from SSM — never in config.
/// This struct holds non-secret settings only.
#[derive(Debug, Clone, Deserialize)]
pub struct NotificationConfig {
    /// Telegram Bot API base URL.
    pub telegram_api_base_url: String,
    /// HTTP send timeout in milliseconds for notification POSTs.
    pub send_timeout_ms: u64,
    /// Enable SMS alerts via AWS SNS for Critical/High severity events.
    /// Phone number is fetched from SSM at `/dlt/{env}/sns/phone-number`.
    pub sns_enabled: bool,
}

impl Default for NotificationConfig {
    fn default() -> Self {
        Self {
            // APPROVED: config default — overridable via TOML config file
            telegram_api_base_url: "https://api.telegram.org".to_string(),
            send_timeout_ms: 10_000,
            sns_enabled: false,
        }
    }
}

/// Observability stack configuration.
///
/// Controls Prometheus metrics export and OpenTelemetry tracing.
/// Metrics are served via an HTTP endpoint for Prometheus to scrape.
/// Traces are exported via OTLP gRPC to Jaeger.
#[derive(Debug, Clone, Deserialize)]
pub struct ObservabilityConfig {
    /// Port for the Prometheus metrics HTTP endpoint served by the application.
    pub metrics_port: u16,
    /// OTLP gRPC endpoint for trace export (e.g., dlt-jaeger:4317).
    pub otlp_endpoint: String,
    /// Enable Prometheus metrics export.
    pub metrics_enabled: bool,
    /// Enable OpenTelemetry trace export to Jaeger.
    pub tracing_enabled: bool,
}

impl Default for ObservabilityConfig {
    fn default() -> Self {
        Self {
            metrics_port: 9091,
            otlp_endpoint: "http://dlt-jaeger:4317".to_string(), // APPROVED: Docker DNS hostname for OTLP collector
            metrics_enabled: true,
            tracing_enabled: true,
        }
    }
}

/// Subscription planner configuration.
///
/// Controls which instruments are subscribed and at what feed mode.
/// Indices get full chain (all expiries, all strikes). Stocks get current
/// expiry only with ATM ± N strike filtering.
#[derive(Debug, Clone, Deserialize)]
pub struct SubscriptionConfig {
    /// Feed mode for all subscriptions. Start with Ticker, upgrade later.
    /// Valid values: "Ticker", "Quote", "Full".
    pub feed_mode: String,

    /// Whether to subscribe index derivatives (full chain, all expiries).
    pub subscribe_index_derivatives: bool,

    /// Whether to subscribe stock derivatives (current expiry, ATM ± N).
    pub subscribe_stock_derivatives: bool,

    /// Whether to subscribe display-only indices (INDIA VIX, sectoral, etc.).
    pub subscribe_display_indices: bool,

    /// Whether to subscribe stock equity price feeds (NSE_EQ segment).
    pub subscribe_stock_equities: bool,

    /// Number of strikes above ATM for stock options.
    pub stock_atm_strikes_above: usize,

    /// Number of strikes below ATM for stock options.
    pub stock_atm_strikes_below: usize,

    /// Default LTP to use for ATM calculation when no live price is available.
    /// When the system first starts, there are no live prices yet.
    /// This fallback ensures we subscribe to a reasonable strike range.
    /// Once live prices arrive, dynamic rebalancing (Phase 2) will adjust.
    pub stock_default_atm_fallback_enabled: bool,
}

impl Default for SubscriptionConfig {
    fn default() -> Self {
        Self {
            feed_mode: "Ticker".to_string(),
            subscribe_index_derivatives: true,
            subscribe_stock_derivatives: true,
            subscribe_display_indices: true,
            subscribe_stock_equities: true,
            stock_atm_strikes_above: 10,
            stock_atm_strikes_below: 10,
            stock_default_atm_fallback_enabled: true,
        }
    }
}

impl SubscriptionConfig {
    /// Parses the feed_mode string into a `FeedMode` enum.
    ///
    /// # Errors
    /// Returns error if the string is not a recognized feed mode.
    pub fn parsed_feed_mode(&self) -> Result<crate::types::FeedMode> {
        match self.feed_mode.as_str() {
            "Ticker" => Ok(crate::types::FeedMode::Ticker),
            "Quote" => Ok(crate::types::FeedMode::Quote),
            "Full" => Ok(crate::types::FeedMode::Full),
            other => bail!(
                "subscription.feed_mode must be Ticker/Quote/Full, got '{}'",
                other
            ),
        }
    }
}

/// Historical data fetching configuration.
///
/// Controls automated fetching of 1-minute OHLCV candles from Dhan's
/// intraday charts API for cross-verification with live tick data.
#[derive(Debug, Clone, Deserialize)]
pub struct HistoricalDataConfig {
    /// Enable automated historical candle fetching after market close.
    pub enabled: bool,
    /// Number of past trading days to fetch on startup for cross-verification.
    /// Dhan allows up to 90 days per request.
    pub lookback_days: u32,
    /// HTTP request timeout in seconds for historical data API calls.
    pub request_timeout_secs: u64,
    /// Maximum retry attempts for failed API requests.
    pub max_retries: u32,
    /// Delay in milliseconds between consecutive API requests (rate limiting).
    pub request_delay_ms: u64,
}

impl Default for HistoricalDataConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            lookback_days: 90,
            request_timeout_secs: 30,
            max_retries: 3,
            request_delay_ms: 500,
        }
    }
}

/// Index constituency download configuration.
///
/// Controls the download of NSE index constituent CSVs from niftyindices.com.
/// Used to build a bidirectional index-stock mapping for the trading system.
#[derive(Debug, Clone, Deserialize)]
pub struct IndexConstituencyConfig {
    /// Whether constituency download is enabled.
    #[serde(default = "default_constituency_enabled")]
    pub enabled: bool,
    /// HTTP request timeout in seconds for individual CSV downloads.
    #[serde(default = "default_constituency_download_timeout_secs")]
    pub download_timeout_secs: u64,
    /// Maximum concurrent CSV downloads.
    #[serde(default = "default_constituency_max_concurrent_downloads")]
    pub max_concurrent_downloads: usize,
    /// Delay in milliseconds between batches of concurrent downloads.
    #[serde(default)]
    pub inter_batch_delay_ms: u64,
}

impl Default for IndexConstituencyConfig {
    fn default() -> Self {
        Self {
            enabled: default_constituency_enabled(),
            download_timeout_secs: default_constituency_download_timeout_secs(),
            max_concurrent_downloads: default_constituency_max_concurrent_downloads(),
            inter_batch_delay_ms: 0,
        }
    }
}

const fn default_constituency_enabled() -> bool {
    true
}

const fn default_constituency_download_timeout_secs() -> u64 {
    30
}

const fn default_constituency_max_concurrent_downloads() -> usize {
    5
}

// ---------------------------------------------------------------------------
// Configuration Validation
// ---------------------------------------------------------------------------

impl ApplicationConfig {
    /// Validates all configuration values at startup.
    ///
    /// Catches invalid time formats, SEBI violations, and nonsensical ranges
    /// before the system starts trading.
    ///
    /// # Errors
    /// Returns descriptive error for the first validation failure found.
    pub fn validate(&self) -> Result<()> {
        // Timezone must be Asia/Kolkata — SEBI requirement.
        if self.trading.timezone != "Asia/Kolkata" {
            bail!(
                "trading.timezone must be 'Asia/Kolkata', got '{}'",
                self.trading.timezone
            );
        }

        // Validate time string formats.
        let time_fields = [
            ("trading.market_open_time", &self.trading.market_open_time),
            ("trading.market_close_time", &self.trading.market_close_time),
            ("trading.order_cutoff_time", &self.trading.order_cutoff_time),
            (
                "trading.data_collection_start",
                &self.trading.data_collection_start,
            ),
            (
                "trading.data_collection_end",
                &self.trading.data_collection_end,
            ),
            (
                "instrument.daily_download_time",
                &self.instrument.daily_download_time,
            ),
            (
                "instrument.build_window_start",
                &self.instrument.build_window_start,
            ),
            (
                "instrument.build_window_end",
                &self.instrument.build_window_end,
            ),
        ];
        for (field_name, value) in &time_fields {
            NaiveTime::parse_from_str(value, "%H:%M:%S").map_err(|_| {
                anyhow::anyhow!("{} is not a valid HH:MM:SS time: '{}'", field_name, value)
            })?;
        }

        // SEBI: max_orders_per_second must not exceed the SEBI limit.
        if self.trading.max_orders_per_second > SEBI_MAX_ORDERS_PER_SECOND {
            bail!(
                "trading.max_orders_per_second ({}) exceeds SEBI limit ({})",
                self.trading.max_orders_per_second,
                SEBI_MAX_ORDERS_PER_SECOND
            );
        }

        // Token: refresh_before_expiry must be less than token validity.
        if self.token.refresh_before_expiry_hours >= self.token.token_validity_hours {
            bail!(
                "token.refresh_before_expiry_hours ({}) must be less than token.token_validity_hours ({})",
                self.token.refresh_before_expiry_hours,
                self.token.token_validity_hours
            );
        }

        // Network: timeouts and retries must be positive.
        if self.network.request_timeout_ms == 0 {
            bail!("network.request_timeout_ms must be > 0");
        }
        if self.network.retry_max_attempts == 0 {
            bail!("network.retry_max_attempts must be > 0");
        }

        // Risk: daily loss must be a finite positive number in (0, 100].
        // NaN/Inf must be rejected — NaN defeats comparisons (all return false)
        // and would corrupt financial calculations.
        if !self.risk.max_daily_loss_percent.is_finite()
            || self.risk.max_daily_loss_percent <= 0.0
            || self.risk.max_daily_loss_percent > 100.0
        {
            bail!(
                "risk.max_daily_loss_percent must be a finite value in (0, 100], got {}",
                self.risk.max_daily_loss_percent
            );
        }

        // Instrument: download timeout must be positive.
        if self.instrument.csv_download_timeout_secs == 0 {
            bail!("instrument.csv_download_timeout_secs must be > 0");
        }

        // Instrument: build window start must be before end.
        let window_start =
            NaiveTime::parse_from_str(&self.instrument.build_window_start, "%H:%M:%S").map_err(
                |_| {
                    anyhow::anyhow!(
                        "instrument.build_window_start already validated above but failed again"
                    )
                },
            )?;
        let window_end = NaiveTime::parse_from_str(&self.instrument.build_window_end, "%H:%M:%S")
            .map_err(|_| {
            anyhow::anyhow!("instrument.build_window_end already validated above but failed again")
        })?;
        if window_start >= window_end {
            bail!(
                "instrument.build_window_start ({}) must be before build_window_end ({})",
                self.instrument.build_window_start,
                self.instrument.build_window_end
            );
        }

        // WebSocket: ping interval must be positive.
        if self.websocket.ping_interval_secs == 0 {
            bail!("websocket.ping_interval_secs must be > 0");
        }

        // WebSocket: subscription batch must be in [1, 100] (Dhan limit).
        if self.websocket.subscription_batch_size == 0
            || self.websocket.subscription_batch_size > crate::constants::SUBSCRIPTION_BATCH_SIZE
        {
            bail!(
                "websocket.subscription_batch_size must be in [1, {}], got {}",
                crate::constants::SUBSCRIPTION_BATCH_SIZE,
                self.websocket.subscription_batch_size
            );
        }

        // WebSocket: reconnect attempts must be positive.
        if self.websocket.reconnect_max_attempts == 0 {
            bail!("websocket.reconnect_max_attempts must be > 0");
        }

        // Dhan: max_websocket_connections must be positive (prevents division-by-zero in pool).
        if self.dhan.max_websocket_connections == 0 {
            bail!("dhan.max_websocket_connections must be > 0");
        }

        // WebSocket: pong_timeout_secs must be positive (used in read timeout calculation).
        if self.websocket.pong_timeout_secs == 0 {
            bail!("websocket.pong_timeout_secs must be > 0");
        }

        // WebSocket: computed read timeout must be reasonable relative to Dhan's
        // 40s server timeout. We allow up to 2× the server timeout as an upper
        // bound — beyond that, the config is likely misconfigured.
        let computed_read_timeout = self
            .websocket
            .ping_interval_secs
            .saturating_mul(u64::from(self.websocket.max_consecutive_pong_failures) + 1)
            .saturating_add(self.websocket.pong_timeout_secs);
        let max_reasonable_timeout = crate::constants::SERVER_PING_TIMEOUT_SECS * 2;
        if computed_read_timeout > max_reasonable_timeout {
            bail!(
                "websocket read timeout ({}s = ping_interval × (max_failures+1) + pong_timeout) \
                 exceeds {}s — Dhan server disconnects at {}s",
                computed_read_timeout,
                max_reasonable_timeout,
                crate::constants::SERVER_PING_TIMEOUT_SECS
            );
        }

        // Notification: send timeout must be positive.
        if self.notification.send_timeout_ms == 0 {
            bail!("notification.send_timeout_ms must be > 0");
        }

        // Trading calendar: validate all holiday date strings parse correctly
        // and none fall on weekends. This also constructs the calendar to verify
        // internal consistency.
        TradingCalendar::from_config(&self.trading)?;

        // Historical: validate if enabled.
        if self.historical.enabled {
            if self.historical.lookback_days == 0
                || self.historical.lookback_days
                    > crate::constants::DHAN_INTRADAY_MAX_DAYS_PER_REQUEST
            {
                bail!(
                    "historical.lookback_days must be in [1, {}], got {}",
                    crate::constants::DHAN_INTRADAY_MAX_DAYS_PER_REQUEST,
                    self.historical.lookback_days
                );
            }
            if self.historical.request_timeout_secs == 0 {
                bail!("historical.request_timeout_secs must be > 0");
            }
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: creates a valid ApplicationConfig for testing.
    /// Modify individual fields to test specific validation failures.
    fn make_valid_config() -> ApplicationConfig {
        ApplicationConfig {
            trading: TradingConfig {
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
                ],
                muhurat_trading_dates: vec![NseHolidayEntry {
                    date: "2026-11-08".to_string(),
                    name: "Diwali 2026".to_string(),
                }],
            },
            dhan: DhanConfig {
                websocket_url: "wss://api-feed.dhan.co".to_string(),
                order_update_websocket_url: "wss://api-order-update.dhan.co".to_string(),
                rest_api_base_url: "https://api.dhan.co/v2".to_string(),
                auth_base_url: "https://auth.dhan.co".to_string(),
                instrument_csv_url: "https://images.dhan.co/api-data/api-scrip-master-detailed.csv"
                    .to_string(),
                instrument_csv_fallback_url: "https://images.dhan.co/api-data/api-scrip-master.csv"
                    .to_string(),
                max_instruments_per_connection: 5000,
                max_websocket_connections: 5,
            },
            websocket: WebSocketConfig {
                ping_interval_secs: 10,
                pong_timeout_secs: 10,
                max_consecutive_pong_failures: 2,
                reconnect_initial_delay_ms: 500,
                reconnect_max_delay_ms: 30000,
                reconnect_max_attempts: 10,
                subscription_batch_size: 100,
                connection_stagger_ms: 10000,
            },
            questdb: QuestDbConfig {
                host: "dlt-questdb".to_string(),
                http_port: 9000,
                pg_port: 8812,
                ilp_port: 9009,
            },
            valkey: ValkeyConfig {
                host: "dlt-valkey".to_string(),
                port: 6379,
                max_connections: 16,
            },
            prometheus: PrometheusConfig {
                host: "dlt-prometheus".to_string(),
                port: 9090,
            },
            network: NetworkConfig {
                request_timeout_ms: 5000,
                websocket_connect_timeout_ms: 10000,
                retry_initial_delay_ms: 100,
                retry_max_delay_ms: 30000,
                retry_max_attempts: 5,
            },
            token: TokenConfig {
                refresh_before_expiry_hours: 1,
                token_validity_hours: 24,
            },
            risk: RiskConfig {
                max_daily_loss_percent: 2.0,
                max_position_size_lots: 100,
            },
            logging: LoggingConfig {
                level: "info".to_string(),
                format: "json".to_string(),
            },
            instrument: InstrumentConfig {
                daily_download_time: "08:55:00".to_string(),
                csv_cache_directory: "/tmp/dlt-cache".to_string(),
                csv_cache_filename: "instruments.csv".to_string(),
                csv_download_timeout_secs: 120,
                build_window_start: "08:25:00".to_string(),
                build_window_end: "08:55:00".to_string(),
            },
            api: ApiConfig {
                host: "0.0.0.0".to_string(),
                port: 3001,
            },
            subscription: SubscriptionConfig::default(),
            notification: NotificationConfig::default(),
            observability: ObservabilityConfig::default(),
            historical: HistoricalDataConfig::default(),
            strategy: StrategyConfig::default(),
        }
    }

    #[test]
    fn test_valid_config_passes_validation() {
        let config = make_valid_config();
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_wrong_timezone_fails() {
        let mut config = make_valid_config();
        config.trading.timezone = "UTC".to_string();
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("Asia/Kolkata"));
    }

    #[test]
    fn test_invalid_time_format_fails() {
        let mut config = make_valid_config();
        config.trading.market_open_time = "9:15".to_string(); // Missing seconds
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("market_open_time"));
    }

    #[test]
    fn test_sebi_order_limit_exceeded_fails() {
        let mut config = make_valid_config();
        config.trading.max_orders_per_second = 11; // SEBI limit is 10
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("SEBI"));
    }

    #[test]
    fn test_sebi_order_limit_at_boundary_passes() {
        let mut config = make_valid_config();
        config.trading.max_orders_per_second = 10; // Exactly at limit
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_token_refresh_exceeds_validity_fails() {
        let mut config = make_valid_config();
        config.token.refresh_before_expiry_hours = 25;
        config.token.token_validity_hours = 24;
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("refresh_before_expiry_hours"));
    }

    #[test]
    fn test_token_refresh_equals_validity_fails() {
        let mut config = make_valid_config();
        config.token.refresh_before_expiry_hours = 24;
        config.token.token_validity_hours = 24;
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("refresh_before_expiry_hours"));
    }

    #[test]
    fn test_zero_request_timeout_fails() {
        let mut config = make_valid_config();
        config.network.request_timeout_ms = 0;
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("request_timeout_ms"));
    }

    #[test]
    fn test_zero_retry_attempts_fails() {
        let mut config = make_valid_config();
        config.network.retry_max_attempts = 0;
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("retry_max_attempts"));
    }

    #[test]
    fn test_zero_daily_loss_percent_fails() {
        let mut config = make_valid_config();
        config.risk.max_daily_loss_percent = 0.0;
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("max_daily_loss_percent"));
    }

    #[test]
    fn test_negative_daily_loss_percent_fails() {
        let mut config = make_valid_config();
        config.risk.max_daily_loss_percent = -5.0;
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("max_daily_loss_percent"));
    }

    #[test]
    fn test_daily_loss_percent_over_100_fails() {
        let mut config = make_valid_config();
        config.risk.max_daily_loss_percent = 100.1;
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("max_daily_loss_percent"));
    }

    #[test]
    fn test_daily_loss_percent_at_100_passes() {
        let mut config = make_valid_config();
        config.risk.max_daily_loss_percent = 100.0;
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_daily_loss_percent_nan_fails() {
        let mut config = make_valid_config();
        config.risk.max_daily_loss_percent = f64::NAN;
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("max_daily_loss_percent"));
    }

    #[test]
    fn test_daily_loss_percent_inf_fails() {
        let mut config = make_valid_config();
        config.risk.max_daily_loss_percent = f64::INFINITY;
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("max_daily_loss_percent"));
    }

    #[test]
    fn test_daily_loss_percent_neg_inf_fails() {
        let mut config = make_valid_config();
        config.risk.max_daily_loss_percent = f64::NEG_INFINITY;
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("max_daily_loss_percent"));
    }

    #[test]
    fn test_zero_csv_download_timeout_fails() {
        let mut config = make_valid_config();
        config.instrument.csv_download_timeout_secs = 0;
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("csv_download_timeout_secs"));
    }

    #[test]
    fn test_invalid_instrument_download_time_fails() {
        let mut config = make_valid_config();
        config.instrument.daily_download_time = "not-a-time".to_string();
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("daily_download_time"));
    }

    // --- WebSocket Config Validation ---

    #[test]
    fn test_websocket_zero_ping_interval_fails() {
        let mut config = make_valid_config();
        config.websocket.ping_interval_secs = 0;
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("ping_interval_secs"));
    }

    #[test]
    fn test_websocket_zero_subscription_batch_size_fails() {
        let mut config = make_valid_config();
        config.websocket.subscription_batch_size = 0;
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("subscription_batch_size"));
    }

    #[test]
    fn test_websocket_subscription_batch_size_over_100_fails() {
        let mut config = make_valid_config();
        config.websocket.subscription_batch_size = 101;
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("subscription_batch_size"));
    }

    #[test]
    fn test_websocket_zero_reconnect_max_attempts_fails() {
        let mut config = make_valid_config();
        config.websocket.reconnect_max_attempts = 0;
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("reconnect_max_attempts"));
    }

    #[test]
    fn test_websocket_subscription_batch_size_exactly_100_passes() {
        let mut config = make_valid_config();
        config.websocket.subscription_batch_size = 100;
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_websocket_subscription_batch_size_exactly_1_passes() {
        let mut config = make_valid_config();
        config.websocket.subscription_batch_size = 1;
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_dhan_zero_max_websocket_connections_fails() {
        let mut config = make_valid_config();
        config.dhan.max_websocket_connections = 0;
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("max_websocket_connections"));
    }

    #[test]
    fn test_websocket_zero_pong_timeout_fails() {
        let mut config = make_valid_config();
        config.websocket.pong_timeout_secs = 0;
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("pong_timeout_secs"));
    }

    #[test]
    fn test_notification_zero_send_timeout_fails() {
        let mut config = make_valid_config();
        config.notification.send_timeout_ms = 0;
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("notification.send_timeout_ms"));
    }

    #[test]
    fn test_build_window_start_after_end_fails() {
        let mut config = make_valid_config();
        config.instrument.build_window_start = "09:00:00".to_string();
        config.instrument.build_window_end = "08:30:00".to_string();
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("build_window_start"));
    }

    #[test]
    fn test_build_window_start_equals_end_fails() {
        let mut config = make_valid_config();
        config.instrument.build_window_start = "08:30:00".to_string();
        config.instrument.build_window_end = "08:30:00".to_string();
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("build_window_start"));
    }

    #[test]
    fn test_invalid_build_window_start_format_fails() {
        let mut config = make_valid_config();
        config.instrument.build_window_start = "8:25".to_string();
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("build_window_start"));
    }

    // -----------------------------------------------------------------------
    // Cross-field and boundary validation tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_market_close_before_open_time_values() {
        // Verify that market_close_time > market_open_time in the default config.
        // The validator doesn't currently reject this cross-field case,
        // but the parsed times should have the expected ordering.
        let config = make_valid_config();
        let open = NaiveTime::parse_from_str(&config.trading.market_open_time, "%H:%M:%S").unwrap();
        let close =
            NaiveTime::parse_from_str(&config.trading.market_close_time, "%H:%M:%S").unwrap();
        assert!(
            close > open,
            "market_close_time ({close}) must be after market_open_time ({open})"
        );
    }

    #[test]
    fn test_order_cutoff_before_close() {
        // Verify that order_cutoff_time < market_close_time in the default config.
        let config = make_valid_config();
        let cutoff =
            NaiveTime::parse_from_str(&config.trading.order_cutoff_time, "%H:%M:%S").unwrap();
        let close =
            NaiveTime::parse_from_str(&config.trading.market_close_time, "%H:%M:%S").unwrap();
        assert!(
            cutoff < close,
            "order_cutoff_time ({cutoff}) must be before market_close_time ({close})"
        );
    }

    #[test]
    fn test_historical_lookback_zero_fails() {
        let mut config = make_valid_config();
        config.historical.enabled = true;
        config.historical.lookback_days = 0;
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("lookback_days"));
    }

    #[test]
    fn test_historical_lookback_over_90_fails() {
        let mut config = make_valid_config();
        config.historical.enabled = true;
        config.historical.lookback_days = 91;
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("lookback_days"));
    }

    #[test]
    fn test_historical_lookback_at_90_passes() {
        let mut config = make_valid_config();
        config.historical.enabled = true;
        config.historical.lookback_days = 90;
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_historical_lookback_at_1_passes() {
        let mut config = make_valid_config();
        config.historical.enabled = true;
        config.historical.lookback_days = 1;
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_historical_request_timeout_zero_fails() {
        let mut config = make_valid_config();
        config.historical.enabled = true;
        config.historical.request_timeout_secs = 0;
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("request_timeout_secs"));
    }

    #[test]
    fn test_historical_disabled_skips_validation() {
        let mut config = make_valid_config();
        config.historical.enabled = false;
        config.historical.lookback_days = 999; // Would fail if validated
        config.historical.request_timeout_secs = 0; // Would fail if validated
        assert!(
            config.validate().is_ok(),
            "disabled historical config should skip validation"
        );
    }

    #[test]
    fn test_websocket_excessive_read_timeout_fails() {
        let mut config = make_valid_config();
        // ping_interval_secs * (max_consecutive_pong_failures + 1) + pong_timeout_secs
        // = 30 * (10 + 1) + 30 = 360s, which exceeds 2 * SERVER_PING_TIMEOUT_SECS
        config.websocket.ping_interval_secs = 30;
        config.websocket.max_consecutive_pong_failures = 10;
        config.websocket.pong_timeout_secs = 30;
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("websocket read timeout"));
    }

    #[test]
    fn test_feed_mode_ticker_passes() {
        let config = SubscriptionConfig {
            feed_mode: "Ticker".to_string(),
            ..SubscriptionConfig::default()
        };
        assert!(config.parsed_feed_mode().is_ok());
    }

    #[test]
    fn test_feed_mode_quote_passes() {
        let config = SubscriptionConfig {
            feed_mode: "Quote".to_string(),
            ..SubscriptionConfig::default()
        };
        assert!(config.parsed_feed_mode().is_ok());
    }

    #[test]
    fn test_feed_mode_full_passes() {
        let config = SubscriptionConfig {
            feed_mode: "Full".to_string(),
            ..SubscriptionConfig::default()
        };
        assert!(config.parsed_feed_mode().is_ok());
    }

    #[test]
    fn test_feed_mode_invalid_string_fails() {
        let config = SubscriptionConfig {
            feed_mode: "invalid".to_string(),
            ..SubscriptionConfig::default()
        };
        let err = config.parsed_feed_mode().unwrap_err();
        assert!(err.to_string().contains("Ticker/Quote/Full"));
    }

    #[test]
    fn test_feed_mode_case_sensitive() {
        let config = SubscriptionConfig {
            feed_mode: "ticker".to_string(), // lowercase — must fail
            ..SubscriptionConfig::default()
        };
        assert!(
            config.parsed_feed_mode().is_err(),
            "feed_mode is case-sensitive — 'ticker' should fail"
        );
    }
}
